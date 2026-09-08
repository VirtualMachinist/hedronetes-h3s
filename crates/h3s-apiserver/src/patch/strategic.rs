//! Native strategic merge using pinned Kubernetes field strategies, not JSON
//! merge's array replacement. All edits are private until API admission/CAS.
use super::super::{resources::Resource, Failure, Result};
use serde_json::{json, Map, Value};
use std::sync::OnceLock;

fn invalid(message: &str) -> Failure {
    Failure::new(422, "Invalid", message)
}
fn schemas() -> &'static Value {
    static SCHEMAS: OnceLock<Value> = OnceLock::new();
    SCHEMAS.get_or_init(|| {
        serde_json::from_str(include_str!("../../proto/strategic-schema.json"))
            .expect("generated strategic schema")
    })
}
fn resolve(schema: &Value) -> &Value {
    schema["$ref"]
        .as_str()
        .and_then(|r| r.strip_prefix("#/definitions/"))
        .map_or(schema, |name| &schemas()["definitions"][name])
}
fn field<'a>(schema: &'a Value, key: &str) -> &'a Value {
    let schema = resolve(schema);
    schema["properties"]
        .get(key)
        .unwrap_or(&schema["additionalProperties"])
}
fn strategy(schema: &Value, expected: &str) -> bool {
    schema["x-kubernetes-patch-strategy"]
        .as_str()
        .is_some_and(|s| s.split(',').any(|s| s == expected))
}
fn merge_key(schema: &Value) -> Option<&str> {
    schema["x-kubernetes-patch-merge-key"].as_str()
}
fn identity<'a>(value: &'a Value, key: Option<&str>) -> Result<&'a Value> {
    let id = if let Some(key) = key {
        value
            .get(key)
            .ok_or_else(|| invalid("strategic list item is missing its merge key"))?
    } else {
        value
    };
    if !id.is_string() && !id.is_number() && !id.is_boolean() {
        return Err(invalid("strategic list keys/items must be scalars"));
    }
    Ok(id)
}

struct Budget(usize);
impl Budget {
    fn spend(&mut self, amount: usize) -> Result<()> {
        self.0 = self.0.checked_sub(amount).ok_or_else(|| {
            Failure::new(
                413,
                "RequestEntityTooLarge",
                "strategic patch work limit exceeded",
            )
        })?;
        Ok(())
    }
}

pub(super) fn apply(resource: Resource, original: Value, patch: Value) -> Result<Value> {
    if !patch.is_object() {
        return Err(invalid("strategic patch must be an object"));
    }
    let root = schemas()["roots"][resource.kind]
        .as_str()
        .expect("every served resource has a generated schema");
    merge(
        original,
        &patch,
        &schemas()["definitions"][root],
        0,
        &mut Budget(1_000_000),
    )
}

fn merge(
    original: Value,
    patch: &Value,
    schema: &Value,
    depth: usize,
    budget: &mut Budget,
) -> Result<Value> {
    budget.spend(1)?;
    if depth > 64 {
        return Err(invalid("strategic patch nesting exceeds 64 levels"));
    }
    match patch {
        Value::Object(patch) => map(original, patch, schema, depth, budget),
        Value::Array(patch) if strategy(schema, "merge") => list(
            original.as_array().map_or(&[], Vec::as_slice),
            patch,
            schema,
            depth,
            budget,
        ),
        _ => Ok(patch.clone()),
    }
}

fn map(
    original: Value,
    patch: &Map<String, Value>,
    schema: &Value,
    depth: usize,
    budget: &mut Budget,
) -> Result<Value> {
    if let Some(directive) = patch.get("$patch") {
        return match directive.as_str() {
            Some("replace") => {
                let mut replacement = patch.clone();
                replacement.remove("$patch");
                Ok(Value::Object(replacement))
            }
            Some("delete") => Ok(json!({})),
            _ => Err(invalid("invalid strategic map $patch directive")),
        };
    }
    let mut result = original.as_object().cloned().unwrap_or_default();
    if let Some(retain) = patch.get("$retainKeys") {
        let retain = retain
            .as_array()
            .ok_or_else(|| invalid("$retainKeys must be a string list"))?;
        budget.spend(retain.len().saturating_mul(result.len() + patch.len()))?;
        if retain.iter().any(|v| !v.is_string())
            || patch.iter().any(|(k, v)| {
                !k.starts_with('$') && !v.is_null() && !retain.iter().any(|v| v.as_str() == Some(k))
            })
        {
            return Err(invalid(
                "$retainKeys must contain every non-null patch field",
            ));
        }
        result.retain(|k, _| retain.iter().any(|v| v.as_str() == Some(k)));
    }
    // Merge ordinary fields before applying parallel deletion/order directives.
    // Parallel directives are consumed for existing fields. Newly introduced
    // subtrees follow Kubernetes's copy-and-prune rule before typed decoding.
    for (key, value) in patch.iter().filter(|(k, _)| !k.starts_with('$')) {
        if value.is_null() {
            result.remove(key);
            continue;
        }
        let old = result.remove(key).unwrap_or(Value::Null);
        let child = field(schema, key);
        if old.is_null() {
            if let Some(value) = fresh(value, depth + 1, budget)? {
                result.insert(key.clone(), value);
            }
        } else {
            result.insert(key.clone(), merge(old, value, child, depth + 1, budget)?);
        }
    }
    for (key, value) in patch.iter().filter(|(k, _)| k.starts_with('$')) {
        if key == "$retainKeys" {
            continue;
        }
        if let Some(name) = key.strip_prefix("$deleteFromPrimitiveList/") {
            let child = field(schema, name);
            if !strategy(child, "merge") || merge_key(child).is_some() {
                return Err(invalid(
                    "primitive deletion requires a primitive merge list",
                ));
            }
            let removals = value
                .as_array()
                .ok_or_else(|| invalid("primitive deletion must be a list"))?;
            for v in removals {
                identity(v, None)?;
            }
            if let Some(old) = result.get_mut(name) {
                let old = old
                    .as_array_mut()
                    .ok_or_else(|| invalid("primitive deletion target is not a list"))?;
                budget.spend(old.len().saturating_mul(removals.len()))?;
                old.retain(|v| !removals.contains(v));
            }
        } else if !key.starts_with("$setElementOrder/") {
            return Err(invalid("unknown strategic patch directive"));
        }
    }
    for (directive, order) in patch
        .iter()
        .filter(|(k, _)| k.starts_with("$setElementOrder/"))
    {
        let name = &directive["$setElementOrder/".len()..];
        let child = field(schema, name);
        if !strategy(child, "merge") {
            return Err(invalid("element ordering requires a merge list"));
        }
        let order = order
            .as_array()
            .ok_or_else(|| invalid("element order must be a list"))?;
        let key = merge_key(child);
        for v in order {
            identity(v, key)?;
        }
        if let Some(values) = patch.get(name) {
            let values = values
                .as_array()
                .ok_or_else(|| invalid("ordered patch must be a list"))?;
            budget.spend(values.len().saturating_mul(order.len()))?;
            let mut position = 0;
            for v in values.iter().filter(|v| v.get("$patch").is_none()) {
                let id = identity(v, key)?;
                let offset = order[position..]
                    .iter()
                    .position(|v| identity(v, key).ok() == Some(id))
                    .ok_or_else(|| invalid("patch list conflicts with $setElementOrder"))?;
                position += offset + 1;
            }
        }
        if let Some(values) = result.remove(name) {
            let values = values
                .as_array()
                .ok_or_else(|| invalid("ordered value must be a list"))?;
            let old: &[Value] = original
                .get(name)
                .and_then(Value::as_array)
                .map_or(&[], Vec::as_slice);
            result.insert(
                name.into(),
                Value::Array(reorder(values, order, old, key, budget)?),
            );
        }
    }
    Ok(Value::Object(result))
}

// When a field is newly added, Kubernetes copies it, pruning null map fields
// and whole map/list items containing $patch. Parallel directives are not
// interpreted inside that new subtree; ordinary typed normalization follows.
fn fresh(value: &Value, depth: usize, budget: &mut Budget) -> Result<Option<Value>> {
    budget.spend(1)?;
    if depth > 64 {
        return Err(invalid("strategic patch nesting exceeds 64 levels"));
    }
    Ok(match value {
        Value::Object(map) if map.contains_key("$patch") => None,
        Value::Object(map) => {
            let mut result = Map::new();
            for (k, v) in map.iter().filter(|(_, v)| !v.is_null()) {
                if let Some(v) = fresh(v, depth + 1, budget)? {
                    result.insert(k.clone(), v);
                }
            }
            Some(Value::Object(result))
        }
        Value::Array(items) => {
            let mut result = Vec::new();
            for item in items {
                if let Some(v) = fresh(item, depth + 1, budget)? {
                    result.push(v);
                }
            }
            Some(Value::Array(result))
        }
        _ => Some(value.clone()),
    })
}

fn list(
    old: &[Value],
    patch: &[Value],
    schema: &Value,
    depth: usize,
    budget: &mut Budget,
) -> Result<Value> {
    budget.spend((old.len() + patch.len()).saturating_mul(patch.len() + 1))?;
    let key = merge_key(schema);
    let mut result = old.to_vec();
    for item in old {
        identity(item, key)?;
    }
    let mut ordinary = Vec::new();
    let mut replace = false;
    for item in patch {
        if let Some(directive) = item.get("$patch") {
            if key.is_none() {
                return Err(invalid("$patch list directives require a keyed list"));
            }
            match directive.as_str() {
                Some("replace") => replace = true,
                Some("delete") => {
                    let id = identity(item, key)?;
                    result.retain(|v| identity(v, key).ok() != Some(id));
                }
                _ => return Err(invalid("invalid strategic list $patch directive")),
            }
        } else {
            identity(item, key)?;
            ordinary.push(item.clone());
        }
    }
    if replace {
        return Ok(Value::Array(ordinary));
    }
    for item in &ordinary {
        let id = identity(item, key)?;
        if let Some(index) = result
            .iter()
            .position(|v| identity(v, key).ok() == Some(id))
        {
            if key.is_some() {
                result[index] = merge(
                    result[index].clone(),
                    item,
                    &resolve(schema)["items"],
                    depth + 1,
                    budget,
                )?;
            }
        } else {
            result.push(item.clone());
        }
    }
    if key.is_none() {
        budget.spend(result.len().saturating_mul(result.len()))?;
        let mut unique = Vec::new();
        for v in result {
            if !unique.contains(&v) {
                unique.push(v);
            }
        }
        result = unique;
    }
    Ok(Value::Array(reorder(&result, &ordinary, old, key, budget)?))
}

/// Preserve requested relative order and untouched server order, interleaving
/// untouched entries according to their previous positions where possible.
fn reorder(
    values: &[Value],
    order: &[Value],
    old: &[Value],
    key: Option<&str>,
    budget: &mut Budget,
) -> Result<Vec<Value>> {
    budget.spend((values.len() + order.len()).saturating_mul(old.len() + order.len() + 1))?;
    for v in values.iter().chain(order).chain(old) {
        identity(v, key)?;
    }
    let position = |v: &Value, list: &[Value]| {
        list.iter()
            .position(|other| identity(v, key).ok() == identity(other, key).ok())
    };
    let (mut requested, mut untouched): (Vec<_>, Vec<_>) = values
        .iter()
        .cloned()
        .partition(|v| position(v, order).is_some());
    requested.sort_by_key(|v| position(v, order));
    untouched.sort_by_key(|v| position(v, old));
    let mut result = Vec::with_capacity(values.len());
    let (mut a, mut b) = (
        untouched.into_iter().peekable(),
        requested.into_iter().peekable(),
    );
    while let (Some(left), Some(right)) = (a.peek(), b.peek()) {
        let before =
            matches!((position(left, old), position(right, old)), (Some(l), Some(r)) if l < r);
        result.push(if before {
            a.next().unwrap()
        } else {
            b.next().unwrap()
        });
    }
    result.extend(a);
    result.extend(b);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resources::RESOURCES;

    #[test]
    fn matches_pinned_kubernetes_oracle() {
        let cases: Vec<Value> =
            serde_json::from_str(include_str!("../../tests/fixtures/strategic-v1.34.11.json"))
                .unwrap();
        assert_eq!(cases.len(), 110);
        for case in cases {
            let resource = *RESOURCES.iter().find(|r| case["kind"] == r.kind).unwrap();
            let actual = apply(resource, case["original"].clone(), case["patch"].clone());
            if case["error"] == true {
                assert!(actual.is_err(), "{}", case["name"]);
            } else {
                assert_eq!(
                    actual.unwrap_or_else(|_| panic!("unexpected error: {}", case["name"])),
                    case["expected"],
                    "{}",
                    case["name"]
                );
            }
        }
        for resource in RESOURCES {
            assert!(schemas()["roots"][resource.kind].is_string());
        }
    }

    #[test]
    fn bounds_work_and_rejects_malformed_directives() {
        let node = *RESOURCES.iter().find(|r| r.kind == "Node").unwrap();
        let conditions: Vec<_> = (0..1100)
            .map(|n| json!({"type":n.to_string(),"status":"True"}))
            .collect();
        let value = json!({"status":{"conditions":conditions}});
        assert!(apply(node, value.clone(), value).is_err());
        for patch in [
            json!([]),
            json!({"status":{"$setElementOrder/conditions":"bad"}}),
            json!({"status":{"$unknown":[]}}),
        ] {
            assert!(apply(node, json!({"status":{}}), patch).is_err());
        }
    }
}
