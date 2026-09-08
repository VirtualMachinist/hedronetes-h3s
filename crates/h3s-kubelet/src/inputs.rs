//! Resolve only references in an assigned Pod through its node identity.
use crate::{invalid, pod, Agent, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use h3s_cri::v1::KeyValue;
use reqwest::Method;
use serde_json::Value;
use std::collections::BTreeMap;
pub(super) async fn data(
    agent: &Agent,
    ns: &str,
    kind: &str,
    name: &str,
    optional: bool,
    binary: bool,
) -> Result<BTreeMap<String, Vec<u8>>> {
    if !pod::safe_component(name) {
        return Err(invalid("invalid referenced object name"));
    }
    let (code, v) = agent
        .request(
            Method::GET,
            &format!("api/v1/namespaces/{ns}/{kind}/{name}"),
            None,
        )
        .await?;
    if code == 404 && optional {
        return Ok(BTreeMap::new());
    }
    if code != 200 {
        return Err(crate::Error::Status(code));
    }
    decode_data(&v, kind == "secrets", binary)
}
fn decode_data(v: &Value, secret: bool, binary: bool) -> Result<BTreeMap<String, Vec<u8>>> {
    let mut out = BTreeMap::new();
    for (key, value) in v["data"].as_object().into_iter().flatten() {
        let value = value
            .as_str()
            .ok_or_else(|| invalid("invalid referenced data"))?;
        let bytes = if secret {
            STANDARD
                .decode(value)
                .map_err(|_| invalid("invalid secret encoding"))?
        } else {
            value.as_bytes().to_vec()
        };
        out.insert(key.clone(), bytes);
    }
    if binary && !secret {
        for (key, value) in v["binaryData"].as_object().into_iter().flatten() {
            let bytes = STANDARD
                .decode(
                    value
                        .as_str()
                        .ok_or_else(|| invalid("invalid binary data"))?,
                )
                .map_err(|_| invalid("invalid binary encoding"))?;
            if out.insert(key.clone(), bytes).is_some() {
                return Err(invalid("duplicate ConfigMap data key"));
            }
        }
    }
    Ok(out)
}
fn env_name(s: &str) -> bool {
    !s.is_empty()
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        && !s.as_bytes()[0].is_ascii_digit()
}
fn env_value(bytes: Vec<u8>) -> Result<String> {
    String::from_utf8(bytes)
        .ok()
        .filter(|s| !s.contains('\0'))
        .ok_or_else(|| invalid("environment data must be UTF-8 without NUL"))
}
pub async fn env(agent: &Agent, p: &Value, c: &Value) -> Result<Vec<KeyValue>> {
    let ns = pod::text(&p["metadata"], "namespace")?;
    let mut vars = BTreeMap::new();
    for source in c["envFrom"].as_array().into_iter().flatten() {
        pod::fields(source, &["prefix", "configMapRef", "secretRef"])?;
        let (key, kind) = if source["secretRef"].is_null() {
            ("configMapRef", "configmaps")
        } else {
            ("secretRef", "secrets")
        };
        if !source["secretRef"].is_null() && !source["configMapRef"].is_null() {
            return Err(invalid("ambiguous environment reference"));
        }
        pod::fields(&source[key], &["name", "optional"])?;
        for (name, value) in data(
            agent,
            ns,
            kind,
            pod::text(&source[key], "name")?,
            source[key]["optional"] == true,
            false,
        )
        .await?
        {
            let name = format!("{}{name}", source["prefix"].as_str().unwrap_or(""));
            if env_name(&name) {
                vars.insert(name, env_value(value)?);
            }
        }
    }
    for e in c["env"].as_array().into_iter().flatten() {
        let name = pod::text(e, "name")?;
        if !env_name(name) {
            return Err(invalid("unsupported environment name"));
        }
        if !e["valueFrom"].is_null() && e["value"].as_str().is_some_and(|s| !s.is_empty()) {
            return Err(invalid("ambiguous environment value"));
        }
        let value = if e["valueFrom"].is_null() {
            expand(e["value"].as_str().unwrap_or(""), &vars)
        } else {
            let source = &e["valueFrom"];
            pod::fields(source, &["configMapKeyRef", "secretKeyRef", "fieldRef"])?;
            if ["configMapKeyRef", "secretKeyRef", "fieldRef"]
                .iter()
                .filter(|k| !source[**k].is_null())
                .count()
                != 1
            {
                return Err(invalid("invalid environment value source"));
            }
            if !source["fieldRef"].is_null() {
                pod::fields(&source["fieldRef"], &["fieldPath", "apiVersion"])?;
                match pod::text(&source["fieldRef"], "fieldPath")? {
                    "metadata.name" => pod::text(&p["metadata"], "name")?.into(),
                    "metadata.namespace" => ns.into(),
                    "metadata.uid" => pod::text(&p["metadata"], "uid")?.into(),
                    "spec.nodeName" => agent.name.clone(),
                    _ => return Err(invalid("unsupported downward API field")),
                }
            } else {
                let (key, kind) = if source["secretKeyRef"].is_null() {
                    ("configMapKeyRef", "configmaps")
                } else {
                    ("secretKeyRef", "secrets")
                };
                let source = &source[key];
                pod::fields(source, &["name", "key", "optional"])?;
                let mut values = data(
                    agent,
                    ns,
                    kind,
                    pod::text(source, "name")?,
                    source["optional"] == true,
                    false,
                )
                .await?;
                match values.remove(pod::text(source, "key")?) {
                    Some(v) => env_value(v)?,
                    None if source["optional"] == true => continue,
                    None => return Err(invalid("referenced environment key is missing")),
                }
            }
        };
        if value.contains('\0') {
            return Err(invalid("environment value contains NUL"));
        }
        vars.insert(name.into(), value);
    }
    Ok(vars
        .into_iter()
        .map(|(key, value)| KeyValue { key, value })
        .collect())
}
pub fn expand(value: &str, vars: &BTreeMap<String, String>) -> String {
    let mut out = String::new();
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('$') => {
                chars.next();
                out.push('$');
            }
            Some('(') => {
                chars.next();
                let mut name = String::new();
                let mut closed = false;
                for c in chars.by_ref() {
                    if c == ')' {
                        closed = true;
                        break;
                    }
                    name.push(c);
                }
                if closed {
                    if let Some(v) = vars.get(&name) {
                        out.push_str(v);
                    } else {
                        out.push_str(&format!("$({name})"));
                    }
                } else {
                    out.push_str(&format!("$({name}"));
                }
            }
            _ => out.push('$'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn config_binary_data_is_for_files_and_secret_data_is_decoded() {
        let v = serde_json::json!({"data":{"text":"value"},"binaryData":{"bytes":"AP8="}});
        let env = decode_data(&v, false, false).unwrap();
        assert_eq!(env.len(), 1);
        let files = decode_data(&v, false, true).unwrap();
        assert_eq!(files["bytes"], [0, 255]);
        let v = serde_json::json!({"data":{"bytes":"AP8="}});
        assert_eq!(decode_data(&v, true, true).unwrap()["bytes"], [0, 255]);
        assert!(decode_data(
            &serde_json::json!({"data":{"bytes":"invalid base64"}}),
            true,
            true
        )
        .is_err());
        assert!(decode_data(
            &serde_json::json!({"data":{"same":"text"},"binaryData":{"same":"AP8="}}),
            false,
            true
        )
        .is_err());
    }
    #[test]
    fn environment_expansion_preserves_unknown_and_escaped_values() {
        let vars = BTreeMap::from([
            ("NAME".into(), "value".into()),
            ("NESTED".into(), "$(NAME)".into()),
        ]);
        assert_eq!(
            expand("$(NAME):$$(NAME):$(MISSING):$(NESTED):$HOME:$", &vars),
            "value:$(NAME):$(MISSING):$(NAME):$HOME:$"
        );
        assert_eq!(expand("$(unclosed", &vars), "$(unclosed");
        assert!(!env_name("bad=name"));
        assert!(env_value(b"secret\0value".to_vec()).is_err());
    }
}
