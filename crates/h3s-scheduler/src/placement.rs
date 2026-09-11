use chrono::{DateTime, Utc};
use h3s_api::quantity::Quantity;
use serde_json::Value;
fn array(v: &Value) -> impl Iterator<Item = &Value> {
    v.as_array().into_iter().flatten()
}
fn populated(v: &Value) -> bool {
    !v.is_null()
        && !matches!(v,Value::Array(a) if a.is_empty())
        && !matches!(v,Value::Object(a) if a.is_empty())
}
pub fn pending(p: &Value) -> bool {
    p["metadata"]["deletionTimestamp"].is_null()
        && !p["spec"]["nodeName"]
            .as_str()
            .is_some_and(|n| !n.is_empty())
        && matches!(
            p["spec"]["schedulerName"].as_str(),
            None | Some("") | Some("default-scheduler")
        )
        && !terminal(p)
}
fn terminal(p: &Value) -> bool {
    matches!(p["status"]["phase"].as_str(), Some("Succeeded" | "Failed"))
}
#[derive(Clone, Copy, Default, Debug, PartialEq)]
struct Resources {
    cpu: i64,
    memory: i64,
    pods: i64,
}
impl Resources {
    fn add(self, other: Self) -> Option<Self> {
        Some(Self {
            cpu: self.cpu.checked_add(other.cpu)?,
            memory: self.memory.checked_add(other.memory)?,
            pods: self.pods.checked_add(other.pods)?,
        })
    }
    fn fits(self, capacity: Self) -> bool {
        self.cpu <= capacity.cpu && self.memory <= capacity.memory && self.pods <= capacity.pods
    }
}
fn requests(p: &Value) -> Option<Resources> {
    let s = &p["spec"];
    if populated(&s["initContainers"])
        || populated(&s["resources"])
        || populated(&s["resourceClaims"])
    {
        return None;
    }
    let containers = s["containers"].as_array().filter(|a| !a.is_empty())?;
    let mut total = Resources {
        pods: 1,
        ..Default::default()
    };
    for c in containers {
        let r = &c["resources"];
        for kind in ["requests", "limits"] {
            if r[kind]
                .as_object()
                .is_some_and(|m| m.keys().any(|k| !matches!(k.as_str(), "cpu" | "memory")))
            {
                return None;
            }
        }
        let mut values = [0, 0];
        for (i, key) in ["cpu", "memory"].iter().enumerate() {
            let convert = |s: &str| {
                let q = Quantity::parse(s)?;
                if i == 0 {
                    q.as_milli_cpu()
                } else {
                    q.as_bytes()
                }
            };
            let limit = r["limits"][key].as_str().map(convert).transpose_option()?;
            let request = r["requests"][key]
                .as_str()
                .map(convert)
                .transpose_option()?
                .or(limit)
                .unwrap_or(0);
            if limit.is_some_and(|l| request > l) {
                return None;
            }
            values[i] = request;
        }
        total = total.add(Resources {
            cpu: values[0],
            memory: values[1],
            pods: 0,
        })?;
    }
    let overhead = &s["overhead"];
    if overhead
        .as_object()
        .is_some_and(|m| m.keys().any(|k| !matches!(k.as_str(), "cpu" | "memory")))
    {
        return None;
    }
    total.add(Resources {
        cpu: overhead["cpu"]
            .as_str()
            .map(|s| Quantity::parse(s).and_then(|q| q.as_milli_cpu()))
            .transpose_option()?
            .unwrap_or(0),
        memory: overhead["memory"]
            .as_str()
            .map(|s| Quantity::parse(s).and_then(|q| q.as_bytes()))
            .transpose_option()?
            .unwrap_or(0),
        pods: 0,
    })
}
trait TransposeOption<T> {
    fn transpose_option(self) -> Option<Option<T>>;
}
impl<T> TransposeOption<T> for Option<Option<T>> {
    fn transpose_option(self) -> Option<Option<T>> {
        match self {
            None => Some(None),
            Some(v) => v.map(Some),
        }
    }
}
fn capacity(n: &Value) -> Option<Resources> {
    let a = &n["status"]["allocatable"];
    Some(Resources {
        cpu: Quantity::parse(a["cpu"].as_str()?)?.as_milli_cpu()?,
        memory: Quantity::parse(a["memory"].as_str()?)?.as_bytes()?,
        pods: a["pods"].as_str()?.parse().ok().filter(|v| *v > 0)?,
    })
}
fn alive(n: &Value, leases: &[Value], now: DateTime<Utc>) -> bool {
    if !n["metadata"]["deletionTimestamp"].is_null() || n["spec"]["unschedulable"] == true {
        return false;
    }
    let conditions = &n["status"]["conditions"];
    if !array(conditions).any(|c| c["type"] == "Ready" && c["status"] == "True")
        || array(conditions).any(|c| {
            matches!(
                c["type"].as_str(),
                Some("MemoryPressure" | "DiskPressure" | "PIDPressure" | "NetworkUnavailable")
            ) && c["status"] != "False"
        })
    {
        return false;
    }
    leases.iter().any(|l| {
        if l["metadata"]["name"] != n["metadata"]["name"]
            || l["spec"]["holderIdentity"] != n["metadata"]["name"]
            || !array(&l["metadata"]["ownerReferences"]).any(|o| {
                o["kind"] == "Node" && o["apiVersion"] == "v1" && o["uid"] == n["metadata"]["uid"]
            })
        {
            return false;
        }
        let Some(time) = l["spec"]["renewTime"]
            .as_str()
            .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
        else {
            return false;
        };
        let duration = l["spec"]["leaseDurationSeconds"]
            .as_i64()
            .unwrap_or(0)
            .clamp(0, 40);
        let age = now.signed_duration_since(time).num_milliseconds();
        age >= 0 && age <= duration * 1000 && duration > 0
    })
}
fn tolerates(p: &Value, t: &Value) -> bool {
    array(&p["spec"]["tolerations"]).any(|v| {
        let effect = v["effect"].as_str().unwrap_or("");
        if !effect.is_empty() && v["effect"] != t["effect"] {
            return false;
        }
        let key = v["key"].as_str().unwrap_or("");
        match v["operator"].as_str().unwrap_or("Equal") {
            "Exists" => key.is_empty() || v["key"] == t["key"],
            "Equal" => {
                !key.is_empty()
                    && v["key"] == t["key"]
                    && v["value"].as_str().unwrap_or("") == t["value"].as_str().unwrap_or("")
            }
            _ => false,
        }
    })
}
fn requirement(r: &Value, value: Option<&str>) -> bool {
    let values: Vec<_> = array(&r["values"]).filter_map(Value::as_str).collect();
    match r["operator"].as_str() {
        Some("In") => !values.is_empty() && value.is_some_and(|v| values.contains(&v)),
        Some("NotIn") => !values.is_empty() && value.is_none_or(|v| !values.contains(&v)),
        Some("Exists") => values.is_empty() && value.is_some(),
        Some("DoesNotExist") => values.is_empty() && value.is_none(),
        Some("Gt" | "Lt") if values.len() == 1 => value
            .and_then(|v| v.parse::<i64>().ok())
            .zip(values[0].parse::<i64>().ok())
            .is_some_and(|(a, b)| if r["operator"] == "Gt" { a > b } else { a < b }),
        _ => false,
    }
}
fn term(t: &Value, n: &Value) -> bool {
    let labels = &n["metadata"]["labels"];
    let expressions: Vec<_> = array(&t["matchExpressions"]).collect();
    let fields: Vec<_> = array(&t["matchFields"]).collect();
    (!expressions.is_empty() || !fields.is_empty())
        && expressions.iter().all(|r| {
            r["key"]
                .as_str()
                .is_some_and(|k| requirement(r, labels[k].as_str()))
        })
        && fields
            .iter()
            .all(|r| r["key"] == "metadata.name" && requirement(r, n["metadata"]["name"].as_str()))
}
fn affinity(p: &Value, n: &Value) -> Option<i64> {
    if p["spec"]["nodeSelector"]
        .as_object()
        .is_some_and(|m| m.iter().any(|(k, v)| n["metadata"]["labels"][k] != *v))
    {
        return None;
    }
    let a = &p["spec"]["affinity"]["nodeAffinity"];
    let required = &a["requiredDuringSchedulingIgnoredDuringExecution"];
    if !required.is_null() && !array(&required["nodeSelectorTerms"]).any(|t| term(t, n)) {
        return None;
    }
    let mut score = 0i64;
    for pref in array(&a["preferredDuringSchedulingIgnoredDuringExecution"]) {
        let weight = pref["weight"].as_i64().filter(|w| (1..=100).contains(w))?;
        if term(&pref["preference"], n) {
            score = score.checked_add(weight)?;
        }
    }
    Some(score)
}
pub fn select(
    p: &Value,
    nodes: &[Value],
    pods: &[Value],
    leases: &[Value],
    now: DateTime<Utc>,
) -> Result<String, &'static str> {
    let s = &p["spec"];
    if populated(&s["schedulingGates"]) {
        return Err("SchedulingGated");
    }
    if populated(&s["affinity"]["podAffinity"])
        || populated(&s["affinity"]["podAntiAffinity"])
        || populated(&s["topologySpreadConstraints"])
        || populated(&s["runtimeClassName"])
        || array(&s["volumes"]).any(|v| {
            ![
                "configMap",
                "secret",
                "projected",
                "downwardAPI",
                "emptyDir",
            ]
            .iter()
            .any(|k| !v[k].is_null())
        })
        || array(&s["containers"])
            .any(|c| array(&c["ports"]).any(|p| p["hostPort"].as_i64().unwrap_or(0) != 0))
    {
        return Err("UnsupportedScheduling");
    }
    let requested = requests(p).ok_or("UnsupportedResources")?;
    let mut choices = Vec::new();
    for node in nodes {
        if !alive(node, leases, now) {
            continue;
        }
        let Some(name) = node["metadata"]["name"].as_str().filter(|n| !n.is_empty()) else {
            continue;
        };
        let Some(capacity) = capacity(node) else {
            continue;
        };
        let Some(preference) = affinity(p, node) else {
            continue;
        };
        let taints = &node["spec"]["taints"];
        if array(taints).any(|t| {
            matches!(t["effect"].as_str(), Some("NoSchedule" | "NoExecute")) && !tolerates(p, t)
        }) {
            continue;
        }
        let soft = array(taints)
            .filter(|t| t["effect"] == "PreferNoSchedule" && !tolerates(p, t))
            .count();
        let used = pods
            .iter()
            .filter(|other| other["spec"]["nodeName"] == name && !terminal(other))
            .try_fold(Resources::default(), |sum, p| sum.add(requests(p)?));
        let Some(used) = used else {
            continue;
        };
        if !used.add(requested).is_some_and(|r| r.fits(capacity)) {
            continue;
        }
        choices.push((
            soft,
            std::cmp::Reverse(preference),
            used.pods,
            name.to_owned(),
        ));
    }
    choices.sort();
    choices
        .into_iter()
        .next()
        .map(|c| c.3)
        .ok_or("Unschedulable")
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn fixture() -> (Value, Value, Value, DateTime<Utc>) {
        let time = chrono::DateTime::<Utc>::from(std::time::SystemTime::now());
        (
            json!({"metadata":{"uid":"pod"},"spec":{"containers":[{"resources":{"limits":{"cpu":"500m","memory":"64Mi"}}}]}}),
            json!({"metadata":{"name":"worker","uid":"node","labels":{"zone":"a","size":"4"}},"spec":{},"status":{"allocatable":{"cpu":"1","memory":"128Mi","pods":"2"},"conditions":[{"type":"Ready","status":"True"}]}}),
            json!({"metadata":{"name":"worker","ownerReferences":[{"apiVersion":"v1","kind":"Node","uid":"node"}]},"spec":{"holderIdentity":"worker","leaseDurationSeconds":40,"renewTime":time.to_rfc3339()}}),
            time,
        )
    }
    #[test]
    fn reservations_limits_defaults_and_terminal_release() {
        let (p, n, l, t) = fixture();
        assert_eq!(
            select(
                &p,
                std::slice::from_ref(&n),
                &[],
                std::slice::from_ref(&l),
                t
            )
            .unwrap(),
            "worker"
        );
        let mut used = p.clone();
        used["spec"]["nodeName"] = "worker".into();
        assert!(select(
            &p,
            std::slice::from_ref(&n),
            std::slice::from_ref(&used),
            std::slice::from_ref(&l),
            t
        )
        .is_ok());
        assert_eq!(
            select(
                &p,
                std::slice::from_ref(&n),
                &[used.clone(), used.clone()],
                std::slice::from_ref(&l),
                t
            ),
            Err("Unschedulable")
        );
        used["status"]["phase"] = "Succeeded".into();
        assert!(select(&p, &[n], &[used.clone(), used], &[l], t).is_ok());
    }
    #[test]
    fn stale_lease_cordon_pressure_and_missing_capacity_reject() {
        let (p, n, l, t) = fixture();
        assert!(select(
            &p,
            std::slice::from_ref(&n),
            &[],
            std::slice::from_ref(&l),
            t + chrono::Duration::seconds(41)
        )
        .is_err());
        let mut bad = l.clone();
        bad["metadata"]["ownerReferences"][0]["uid"] = "old-node".into();
        assert!(select(&p, std::slice::from_ref(&n), &[], &[bad], t).is_err());
        let mut cordoned = n.clone();
        cordoned["spec"]["unschedulable"] = true.into();
        let mut deleted = n.clone();
        deleted["metadata"]["deletionTimestamp"] = "now".into();
        let mut missing = n.clone();
        missing["status"]["allocatable"] = Value::Null;
        let mut pressure = n.clone();
        pressure["status"]["conditions"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type":"MemoryPressure","status":"True"}));
        for bad in [cordoned, deleted, missing, pressure] {
            assert!(select(&p, &[bad], &[], std::slice::from_ref(&l), t).is_err());
        }
    }
    #[test]
    fn selectors_affinity_and_tolerations_are_enforced() {
        let (mut p, mut n, l, t) = fixture();
        p["spec"]["nodeSelector"] = json!({"zone":"b"});
        assert!(select(
            &p,
            std::slice::from_ref(&n),
            &[],
            std::slice::from_ref(&l),
            t
        )
        .is_err());
        p["spec"]["nodeSelector"] = json!({"zone":"a"});
        p["spec"]["affinity"] = json!({"nodeAffinity":{"requiredDuringSchedulingIgnoredDuringExecution":{"nodeSelectorTerms":[{"matchExpressions":[{"key":"size","operator":"Gt","values":["2"]}]}]}}});
        assert!(select(
            &p,
            std::slice::from_ref(&n),
            &[],
            std::slice::from_ref(&l),
            t
        )
        .is_ok());
        n["spec"]["taints"] = json!([{"key":"dedicated","value":"lab","effect":"NoSchedule"}]);
        assert!(select(
            &p,
            std::slice::from_ref(&n),
            &[],
            std::slice::from_ref(&l),
            t
        )
        .is_err());
        p["spec"]["tolerations"] =
            json!([{"key":"dedicated","operator":"Equal","value":"lab","effect":"NoSchedule"}]);
        assert!(select(&p, &[n], &[], &[l], t).is_ok());
    }
    #[test]
    fn unsupported_constraints_gates_and_resources_do_not_bind() {
        let (p, n, l, t) = fixture();
        for (key, value) in [
            ("schedulingGates", json!([{"name":"gate"}])),
            ("topologySpreadConstraints", json!([{}])),
            (
                "affinity",
                json!({"podAntiAffinity":{"requiredDuringSchedulingIgnoredDuringExecution":[{}]}}),
            ),
            ("initContainers", json!([{}])),
        ] {
            let mut bad = p.clone();
            bad["spec"][key] = value;
            assert!(select(
                &bad,
                std::slice::from_ref(&n),
                &[],
                std::slice::from_ref(&l),
                t
            )
            .is_err());
        }
        let mut bad = p.clone();
        bad["spec"]["containers"][0]["resources"]["requests"] = json!({"cpu":"2"});
        assert!(select(&bad, &[n], &[], &[l], t).is_err());
    }
}
