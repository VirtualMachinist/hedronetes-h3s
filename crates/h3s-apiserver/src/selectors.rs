//! Kubernetes label and field selectors, shared by LIST and WATCH.
use super::{bad, Result};
use serde_json::Value;

#[derive(Clone, Debug, Default)]
pub(crate) struct Selection {
    labels: Vec<Requirement>,
    fields: Vec<Requirement>,
}
#[derive(Clone, Debug)]
struct Requirement {
    key: String,
    op: Op,
}
#[derive(Clone, Debug)]
enum Op {
    In(Vec<String>),
    NotIn(Vec<String>),
    Exists,
    Absent,
    Greater(i64),
    Less(i64),
}
impl Requirement {
    fn matches(&self, value: Option<&str>) -> bool {
        match &self.op {
            Op::In(values) => value.is_some_and(|v| values.iter().any(|wanted| wanted == v)),
            Op::NotIn(values) => value.is_none_or(|v| !values.iter().any(|wanted| wanted == v)),
            Op::Exists => value.is_some(),
            Op::Absent => value.is_none(),
            Op::Greater(n) => value
                .and_then(|v| v.parse::<i64>().ok())
                .is_some_and(|v| v > *n),
            Op::Less(n) => value
                .and_then(|v| v.parse::<i64>().ok())
                .is_some_and(|v| v < *n),
        }
    }
}
impl Selection {
    pub fn parse(labels: &str, fields: &str, kind: &str) -> Result<Self> {
        let labels = split_labels(labels)?
            .into_iter()
            .map(parse_label)
            .collect::<Result<_>>()?;
        let fields = split_fields(fields)?
            .into_iter()
            .map(|s| parse_field(&s, kind))
            .collect::<Result<_>>()?;
        Ok(Self { labels, fields })
    }
    pub fn exact_name(&self) -> Option<&str> {
        self.exact_field("metadata.name")
    }
    pub fn exact_field(&self, field: &str) -> Option<&str> {
        self.fields.iter().find_map(|r| match &r.op {
            Op::In(values) if r.key == field && values.len() == 1 => Some(values[0].as_str()),
            _ => None,
        })
    }
    pub fn matches(&self, value: &Value) -> bool {
        self.labels
            .iter()
            .all(|r| r.matches(value["metadata"]["labels"][&r.key].as_str()))
            && self
                .fields
                .iter()
                .all(|r| r.matches(Some(&field_value(value, &r.key))))
    }
    pub fn with_field(mut self, field: &str, value: &str) -> Self {
        self.fields.push(Requirement {
            key: field.into(),
            op: Op::In(vec![value.into()]),
        });
        self
    }
    pub fn with_name(mut self, name: Option<&str>) -> Self {
        if let Some(name) = name {
            self.fields.push(Requirement {
                key: "metadata.name".into(),
                op: Op::In(vec![name.into()]),
            });
        }
        self
    }
}
fn field_value(value: &Value, path: &str) -> String {
    let mut current = value;
    for segment in path.split('.') {
        current = &current[segment];
    }
    match current {
        Value::String(s) => s.clone(),
        Value::Bool(v) => v.to_string(),
        Value::Number(v) => v.to_string(),
        _ => String::new(),
    }
}
fn split_labels(s: &str) -> Result<Vec<&str>> {
    if s.trim().is_empty() {
        return Ok(vec![]);
    }
    if s.len() > 8192 {
        return Err(bad("label selector exceeds limit"));
    }
    let mut out = vec![];
    let mut depth = 0;
    let mut start = 0;
    for (i, c) in s.char_indices() {
        match c {
            '(' => {
                if depth != 0 {
                    return Err(bad("nested label selector set"));
                }
                depth = 1;
            }
            ')' => {
                if depth != 1 {
                    return Err(bad("unmatched selector parenthesis"));
                }
                depth = 0;
            }
            ',' if depth == 0 => {
                out.push(s[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    if depth != 0 {
        return Err(bad("unclosed selector set"));
    }
    out.push(s[start..].trim());
    if out.iter().any(|s| s.is_empty()) {
        return Err(bad("empty selector requirement"));
    }
    Ok(out)
}
fn label_part(s: &str, empty: bool) -> bool {
    (empty && s.is_empty())
        || (!s.is_empty()
            && s.len() <= 63
            && s.as_bytes()[0].is_ascii_alphanumeric()
            && s.as_bytes()[s.len() - 1].is_ascii_alphanumeric()
            && s.bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c)))
}
fn label_key(key: &str) -> bool {
    match key.split_once('/') {
        Some((prefix, name)) => {
            !prefix.is_empty()
                && prefix.len() <= 253
                && prefix.split('.').all(|s| {
                    !s.is_empty()
                        && s.len() <= 63
                        && s.as_bytes()[0].is_ascii_alphanumeric()
                        && s.as_bytes()[s.len() - 1].is_ascii_alphanumeric()
                        && s.bytes()
                            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
                })
                && label_part(name, false)
        }
        None => label_part(key, false),
    }
}
fn parse_label(s: &str) -> Result<Requirement> {
    let (key, rest, absent) = if let Some(s) = s.strip_prefix('!') {
        (s.trim(), "", true)
    } else {
        let end = s
            .find(|c: char| c.is_whitespace() || "=!<>()".contains(c))
            .unwrap_or(s.len());
        (&s[..end], s[end..].trim(), false)
    };
    if !label_key(key) {
        return Err(bad("invalid label selector key"));
    }
    let op = if absent {
        Op::Absent
    } else if rest.is_empty() {
        Op::Exists
    } else if let Some(values) = rest.strip_prefix("notin") {
        Op::NotIn(parse_set(values)?)
    } else if let Some(values) = rest.strip_prefix("in") {
        Op::In(parse_set(values)?)
    } else if let Some(value) = rest.strip_prefix("!=") {
        Op::NotIn(vec![parse_value(value)?])
    } else if let Some(value) = rest.strip_prefix("==").or_else(|| rest.strip_prefix('=')) {
        Op::In(vec![parse_value(value)?])
    } else if let Some(value) = rest.strip_prefix('>') {
        Op::Greater(
            parse_value(value)?
                .parse()
                .map_err(|_| bad("selector comparison needs an integer"))?,
        )
    } else if let Some(value) = rest.strip_prefix('<') {
        Op::Less(
            parse_value(value)?
                .parse()
                .map_err(|_| bad("selector comparison needs an integer"))?,
        )
    } else {
        return Err(bad("invalid label selector operator"));
    };
    Ok(Requirement {
        key: key.into(),
        op,
    })
}
fn parse_value(value: &str) -> Result<String> {
    let value = value.trim();
    if !label_part(value, true) {
        return Err(bad("invalid label selector value"));
    }
    Ok(value.into())
}
fn parse_set(value: &str) -> Result<Vec<String>> {
    let inside = value
        .trim()
        .strip_prefix('(')
        .and_then(|v| v.strip_suffix(')'))
        .ok_or_else(|| bad("selector in/notin requires parentheses"))?;
    inside.split(',').map(parse_value).collect()
}
fn split_fields(s: &str) -> Result<Vec<String>> {
    if s.trim().is_empty() {
        return Ok(vec![]);
    }
    if s.len() > 8192 {
        return Err(bad("field selector exceeds limit"));
    }
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut escaped = false;
    for c in s.chars() {
        if escaped {
            if !matches!(c, ',' | '=' | '\\') {
                return Err(bad("invalid field selector escape"));
            }
            current.push('\\');
            current.push(c);
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == ',' {
            parts.push(std::mem::take(&mut current));
        } else {
            current.push(c);
        }
    }
    if escaped {
        return Err(bad("trailing selector escape"));
    }
    parts.push(current);
    Ok(parts)
}
fn unescape(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        out.push(if c == '\\' { chars.next().unwrap() } else { c });
    }
    out
}
fn parse_field(s: &str, kind: &str) -> Result<Requirement> {
    let s = s.trim();
    let mut escaped = false;
    let mut operator = None;
    for (i, c) in s.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if c == '\\' {
            escaped = true;
            continue;
        }
        if s[i..].starts_with("!=") {
            operator = Some((i, 2, true));
            break;
        }
        if c == '=' {
            operator = Some((i, if s[i..].starts_with("==") { 2 } else { 1 }, false));
            break;
        }
    }
    let (pos, len, negated) = operator.ok_or_else(|| bad("field selector requires =, == or !="))?;
    let key = unescape(s[..pos].trim());
    let value = unescape(s[pos + len..].trim());
    let supported = matches!(key.as_str(), "metadata.name" | "metadata.namespace")
        || match kind {
            "Pod" => matches!(
                key.as_str(),
                "spec.nodeName"
                    | "spec.restartPolicy"
                    | "spec.schedulerName"
                    | "spec.serviceAccountName"
                    | "status.phase"
                    | "status.podIP"
                    | "status.nominatedNodeName"
            ),
            "Node" => key == "spec.unschedulable",
            "Namespace" => key == "status.phase",
            "Secret" => key == "type",
            "Service" => matches!(key.as_str(), "spec.clusterIP" | "spec.type"),
            "ReplicaSet" => key == "status.replicas",
            _ => false,
        };
    if !supported {
        return Err(bad("unsupported field selector for this resource"));
    }
    Ok(Requirement {
        key,
        op: if negated {
            Op::NotIn(vec![value])
        } else {
            Op::In(vec![value])
        },
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn label_set_absence_empty_values_and_numeric_comparisons() {
        let obj = json!({"metadata":{"labels":{"app":"web","tier":"","version":"12","example.org/role":"api"}}});
        for (expr, expected) in [
            ("app=web", true),
            ("app==web", true),
            ("app!=worker", true),
            ("missing!=x", true),
            ("missing notin (x)", true),
            ("missing in (x)", false),
            ("app in (web,worker),!disabled", true),
            ("tier in ()", true),
            ("tier notin ()", false),
            ("example.org/role,version>10", true),
            ("version<12", false),
            ("!app", false),
        ] {
            assert_eq!(
                Selection::parse(expr, "", "ConfigMap")
                    .unwrap()
                    .matches(&obj),
                expected,
                "{expr}"
            );
        }
        for invalid in [
            "a in (x",
            "a in ((x))",
            "a,,b",
            "a,",
            "a===b",
            "bad/key/extra=x",
            "!a=x",
            "a in (bad value)",
        ] {
            assert!(
                Selection::parse(invalid, "", "ConfigMap").is_err(),
                "{invalid}"
            );
        }
    }
    #[test]
    fn field_equality_escaping_and_resource_boundaries() {
        let obj =
            json!({"metadata":{"name":"web","namespace":"demo"},"spec":{"nodeName":"node-a"}});
        let selected =
            Selection::parse("", "metadata.name=web,spec.nodeName==node-a", "Pod").unwrap();
        assert!(selected.matches(&obj));
        assert_eq!(selected.exact_name(), Some("web"));
        assert!(!Selection::parse("", "metadata.namespace!=demo", "Pod")
            .unwrap()
            .matches(&obj));
        assert!(Selection::parse("", "spec.nodeName=node-a", "ConfigMap").is_err());
        assert!(Selection::parse("", "metadata.name=bad\\q", "Pod").is_err());
        assert_eq!(
            Selection::parse("", "metadata.name=a\\,b\\=c", "Pod")
                .unwrap()
                .exact_name(),
            Some("a,b=c")
        );
    }
}
