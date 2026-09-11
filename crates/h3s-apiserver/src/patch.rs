//! Bounded RFC 6902 / RFC 7396 transformations before the ordinary write path.
use super::{resources::Resource, Failure, Result};
use serde_json::Value;
mod strategic;

const MAX_OBJECT_BYTES: usize = 2 * 1024 * 1024;

pub(crate) fn apply(
    resource: Resource,
    mut object: Value,
    bytes: &[u8],
    content_type: &str,
) -> Result<Value> {
    match content_type.split(';').next().unwrap_or("").trim() {
        "application/strategic-merge-patch+json" => {
            object = strategic::apply(resource, object, serde_json::from_slice(bytes)?)?;
            check_size(&object)?;
        }
        "application/merge-patch+json" => {
            let patch: Value = serde_json::from_slice(bytes)?;
            json_patch::merge(&mut object, &patch);
            check_size(&object)?;
        }
        "application/json-patch+json" => {
            let patch: json_patch::Patch = serde_json::from_slice(bytes)?;
            if patch.0.len() > 256 {
                return Err(Failure::new(413, "RequestEntityTooLarge", "JSON patch exceeds 256 operations"));
            }
            // A series of copy operations can grow far beyond the input size.
            // Bound every intermediate value, not just the final object. Changes
            // remain local until all operations and API validation succeed.
            for operation in &patch.0 {
                json_patch::patch(&mut object, std::slice::from_ref(operation))
                    .map_err(|_| Failure::new(422, "Invalid", "JSON patch operation failed"))?;
                check_size(&object)?;
            }
        }
        // Server-side apply is its own verb, not a fourth patch codec.
        "application/apply-patch+json" | "application/apply-patch+yaml" => {
            return Err(Failure::new(
                501,
                "NotImplemented",
                "server-side apply is not implemented",
            ))
        }
        _ => return Err(Failure::new(415, "UnsupportedMediaType",
            "supported patch types: application/json-patch+json, application/merge-patch+json, application/strategic-merge-patch+json")),
    }
    Ok(object)
}

pub(crate) fn check_size(value: &Value) -> Result<()> {
    // The patched object can exceed the request-body limit even for small input.
    if serde_json::to_vec(value)?.len() > MAX_OBJECT_BYTES {
        return Err(Failure::new(
            413,
            "RequestEntityTooLarge",
            "resulting object exceeds 2 MiB",
        ));
    }
    Ok(())
}
