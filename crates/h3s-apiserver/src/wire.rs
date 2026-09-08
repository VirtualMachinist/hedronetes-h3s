//! Kubernetes Protobuf envelope decoding from pinned upstream descriptors.
//! Kubernetes JSON differs from protobuf JSON for Time, Quantity, IntOrString,
//! raw JSON fields, and 64-bit integers; translate those before typed validation.
use super::{bad, Failure, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use prost_reflect::{DescriptorPool, DynamicMessage, MapKey, ReflectMessage, Value as Pb};
use serde_json::{json, Map, Value};
use std::sync::LazyLock;

static POOL: LazyLock<DescriptorPool> = LazyLock::new(|| {
    DescriptorPool::decode(include_bytes!("../proto/kubernetes-v1.34.bin").as_ref())
        .expect("checked-in Kubernetes descriptor")
});
fn field(m: &DynamicMessage, name: &str) -> Pb {
    m.get_field_by_name(name)
        .expect("pinned descriptor field")
        .into_owned()
}
fn string(m: &DynamicMessage, name: &str) -> Result<String> {
    match field(m, name) {
        Pb::String(s) => Ok(s),
        _ => Err(bad("invalid protobuf string")),
    }
}
fn bytes(m: &DynamicMessage, name: &str) -> Result<prost::bytes::Bytes> {
    match field(m, name) {
        Pb::Bytes(b) => Ok(b),
        _ => Err(bad("invalid protobuf bytes")),
    }
}
fn decode_message(name: &str, raw: &[u8]) -> Result<DynamicMessage> {
    let descriptor = POOL
        .get_message_by_name(name)
        .ok_or_else(|| bad("unsupported protobuf kind"))?;
    DynamicMessage::decode(descriptor, raw).map_err(|_| bad("invalid protobuf message"))
}
pub(crate) fn decode(raw: &[u8], content_type: &str) -> Result<Value> {
    if raw.is_empty() {
        return Ok(json!({}));
    }
    match content_type.split(';').next().unwrap_or("").trim() {
        "" | "application/json" => Ok(serde_json::from_slice(raw)?),
        "application/vnd.kubernetes.protobuf" => {
            let raw = raw
                .strip_prefix(b"k8s\0")
                .ok_or_else(|| bad("missing Kubernetes protobuf prefix"))?;
            let envelope = decode_message("k8s.io.apimachinery.pkg.runtime.Unknown", raw)?;
            if !string(&envelope, "contentEncoding")?.is_empty() {
                return Err(bad("encoded protobuf envelope is unsupported"));
            }
            let meta = match field(&envelope, "typeMeta") {
                Pb::Message(m) => m,
                _ => return Err(bad("missing protobuf type metadata")),
            };
            let version = string(&meta, "apiVersion")?;
            let kind = string(&meta, "kind")?;
            let message =
                if kind == "DeleteOptions" && (version == "v1" || version == "meta.k8s.io/v1") {
                    "k8s.io.apimachinery.pkg.apis.meta.v1.DeleteOptions".to_owned()
                } else {
                    let group = match version.as_str() {
                        "v1" => "core",
                        "rbac.authorization.k8s.io/v1" => "rbac",
                        _ => return Err(bad("unsupported protobuf API version")),
                    };
                    if !super::resources::RESOURCES
                        .iter()
                        .any(|r| r.kind == kind && r.api_version() == version)
                    {
                        return Err(bad("unsupported protobuf kind"));
                    }
                    format!("k8s.io.api.{group}.v1.{kind}")
                };
            let raw = bytes(&envelope, "raw")?;
            let mut value = match string(&envelope, "contentType")?.as_str() {
                "" | "application/vnd.kubernetes.protobuf" => to_json(
                    &Pb::Message(decode_message(&message, &raw)?),
                    0,
                    &mut 100_000,
                )?,
                "application/json" => serde_json::from_slice(&raw)?,
                _ => return Err(bad("unsupported embedded protobuf content type")),
            };
            if !value.is_object() {
                return Err(bad("protobuf resource must be an object"));
            }
            value["apiVersion"] = version.into();
            value["kind"] = kind.into();
            Ok(value)
        }
        _ => Err(Failure::new(
            415,
            "UnsupportedMediaType",
            "supported request types are JSON and Kubernetes Protobuf",
        )),
    }
}
fn to_json(value: &Pb, depth: usize, budget: &mut usize) -> Result<Value> {
    if depth > 64 || *budget == 0 {
        return Err(bad("protobuf object exceeds conversion limits"));
    }
    *budget -= 1;
    Ok(match value {
        Pb::Bool(v) => json!(v),
        Pb::I32(v) => json!(v),
        Pb::I64(v) => json!(v),
        Pb::U32(v) => json!(v),
        Pb::U64(v) => json!(v),
        Pb::F32(v) => json!(v),
        Pb::F64(v) => json!(v),
        Pb::EnumNumber(v) => json!(v),
        Pb::String(v) => json!(v),
        Pb::Bytes(v) => json!(STANDARD.encode(v)),
        Pb::List(items) => Value::Array(
            items
                .iter()
                .map(|v| to_json(v, depth + 1, budget))
                .collect::<Result<_>>()?,
        ),
        Pb::Map(items) => {
            let mut map = Map::new();
            for (k, v) in items {
                let key = match k {
                    MapKey::String(s) => s.clone(),
                    MapKey::Bool(v) => v.to_string(),
                    MapKey::I32(v) => v.to_string(),
                    MapKey::I64(v) => v.to_string(),
                    MapKey::U32(v) => v.to_string(),
                    MapKey::U64(v) => v.to_string(),
                };
                map.insert(key, to_json(v, depth + 1, budget)?);
            }
            Value::Object(map)
        }
        Pb::Message(message) => match message.descriptor().full_name() {
            "k8s.io.apimachinery.pkg.apis.meta.v1.Time"
            | "k8s.io.apimachinery.pkg.apis.meta.v1.MicroTime" => {
                // Empty protobuf Time is Go's zero time, rendered as JSON null.
                if !message.has_field_by_name("seconds") && !message.has_field_by_name("nanos") {
                    Value::Null
                } else {
                    let seconds = match field(message, "seconds") {
                        Pb::I64(s) => s,
                        _ => return Err(bad("invalid timestamp")),
                    };
                    let nanos = match field(message, "nanos") {
                        Pb::I32(n) if (0..1_000_000_000).contains(&n) => n,
                        _ => return Err(bad("invalid timestamp")),
                    };
                    let time = time::OffsetDateTime::from_unix_timestamp(seconds)
                        .map_err(|_| bad("timestamp out of range"))?
                        + time::Duration::nanoseconds(nanos.into());
                    json!(time
                        .format(&time::format_description::well_known::Rfc3339)
                        .map_err(|_| bad("invalid timestamp"))?)
                }
            }
            "k8s.io.apimachinery.pkg.api.resource.Quantity" => json!(string(message, "string")?),
            "k8s.io.apimachinery.pkg.util.intstr.IntOrString" => match field(message, "type") {
                Pb::I64(0) => to_json(&field(message, "intVal"), depth + 1, budget)?,
                Pb::I64(1) => json!(string(message, "strVal")?),
                _ => return Err(bad("invalid IntOrString type")),
            },
            "k8s.io.apimachinery.pkg.apis.meta.v1.FieldsV1" => {
                serde_json::from_slice(&bytes(message, "Raw")?)?
            }
            "k8s.io.apimachinery.pkg.runtime.RawExtension" => {
                serde_json::from_slice(&bytes(message, "raw")?)?
            }
            _ => {
                let mut map = Map::new();
                for (f, v) in message.fields() {
                    map.insert(f.name().into(), to_json(v, depth + 1, budget)?);
                }
                Value::Object(map)
            }
        },
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;
    #[test]
    fn upstream_envelope_decodes_namespace_and_empty_timestamp() {
        let mut metadata = DynamicMessage::new(
            POOL.get_message_by_name("k8s.io.apimachinery.pkg.apis.meta.v1.ObjectMeta")
                .unwrap(),
        );
        metadata.set_field_by_name("name", Pb::String("protobuf-test".into()));
        metadata.set_field_by_name(
            "creationTimestamp",
            Pb::Message(DynamicMessage::new(
                POOL.get_message_by_name("k8s.io.apimachinery.pkg.apis.meta.v1.Time")
                    .unwrap(),
            )),
        );
        let mut namespace = DynamicMessage::new(
            POOL.get_message_by_name("k8s.io.api.core.v1.Namespace")
                .unwrap(),
        );
        namespace.set_field_by_name("metadata", Pb::Message(metadata));
        let mut meta = DynamicMessage::new(
            POOL.get_message_by_name("k8s.io.apimachinery.pkg.runtime.TypeMeta")
                .unwrap(),
        );
        meta.set_field_by_name("apiVersion", Pb::String("v1".into()));
        meta.set_field_by_name("kind", Pb::String("Namespace".into()));
        let mut envelope = DynamicMessage::new(
            POOL.get_message_by_name("k8s.io.apimachinery.pkg.runtime.Unknown")
                .unwrap(),
        );
        envelope.set_field_by_name("typeMeta", Pb::Message(meta));
        envelope.set_field_by_name("raw", Pb::Bytes(namespace.encode_to_vec().into()));
        let mut wire = b"k8s\0".to_vec();
        wire.extend(envelope.encode_to_vec());
        let v = decode(&wire, "application/vnd.kubernetes.protobuf").unwrap();
        assert_eq!(v["metadata"]["name"], "protobuf-test");
        assert!(v["metadata"]["creationTimestamp"].is_null());
        assert_eq!(v["kind"], "Namespace");
        assert!(decode(&wire[4..], "application/vnd.kubernetes.protobuf").is_err());
        assert!(decode(b"payload", "text/plain").is_err());
    }
}
