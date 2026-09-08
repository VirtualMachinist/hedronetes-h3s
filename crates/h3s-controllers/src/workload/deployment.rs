use super::*;
use futures_util::StreamExt;
use kube::runtime::{controller::Action, watcher, Controller};
use sha2::{Digest, Sha256};
use std::sync::Arc;
const HASH: &str = "pod-template-hash";
const REVISION: &str = "deployment.kubernetes.io/revision";

pub async fn run_deployment_controller(client: Client) -> Result<(), Error> {
    Controller::new(
        Api::<Deployment>::all(client.clone()),
        watcher::Config::default(),
    )
    .owns(
        Api::<ReplicaSet>::all(client.clone()),
        watcher::Config::default(),
    )
    .run(
        |d, client: Arc<Client>| async move {
            deployment_once(
                (*client).clone(),
                &d.namespace()
                    .ok_or(Error::Invalid("Deployment namespace missing"))?,
                &d.name_any(),
            )
            .await?;
            Ok(Action::requeue(Duration::from_secs(2)))
        },
        |_, _: &Error, _| Action::requeue(Duration::from_secs(5)),
        Arc::new(client),
    )
    .for_each(|r| async move {
        if let Err(e) = r {
            eprintln!("Deployment controller: {e}");
        }
    })
    .await;
    Err(Error::Stopped)
}
fn template_without_hash(t: &Value) -> Value {
    let mut t = t.clone();
    if let Some(labels) = t["metadata"]["labels"].as_object_mut() {
        labels.remove(HASH);
    }
    t
}
fn current(sets: &[Value], deployment: &Value) -> Option<usize> {
    sets.iter().position(|r| {
        template_without_hash(&r["spec"]["template"])
            == template_without_hash(&deployment["spec"]["template"])
    })
}
fn observed(rs: &Value) -> bool {
    rs["status"]["observedGeneration"] == rs["metadata"]["generation"]
}
fn count(rs: &Value, field: &str) -> i64 {
    rs["status"][field].as_i64().unwrap_or(0).max(0)
}
fn fraction(v: &Value, desired: i64, ceil: bool) -> Result<i64, Error> {
    if let Some(n) = v.as_i64().filter(|n| *n >= 0) {
        return Ok(n);
    }
    let percent = v
        .as_str()
        .and_then(|s| s.strip_suffix('%'))
        .and_then(|s| s.parse::<i64>().ok())
        .filter(|n| *n >= 0)
        .ok_or(Error::Invalid("invalid rollout percentage"))?;
    let product = desired
        .checked_mul(percent)
        .ok_or(Error::Invalid("rollout percentage overflow"))?;
    Ok(product / 100 + i64::from(ceil && product % 100 != 0))
}
fn bounds(d: &Value) -> Result<(i64, i64), Error> {
    let desired = replicas(d)?;
    let update = &d["spec"]["strategy"]["rollingUpdate"];
    let surge = fraction(&update["maxSurge"], desired, true)?;
    let unavailable = fraction(&update["maxUnavailable"], desired, false)?.min(desired);
    if desired > 0 && surge == 0 && unavailable == 0 {
        return Err(Error::Invalid(
            "rollout requires nonzero maxSurge or maxUnavailable",
        ));
    }
    desired
        .checked_add(surge)
        .ok_or(Error::Invalid("rollout capacity overflow"))?;
    Ok((surge, unavailable))
}
/// One change at a time, waiting for the ReplicaSet's generation to be observed.
/// Unavailable old replicas can be removed; available replicas are retained until
/// the aggregate availability budget permits their removal.
fn plan(d: &Value, sets: &[Value], new: usize) -> Result<Option<(usize, i64)>, Error> {
    let desired = replicas(d)?;
    if d["spec"]["paused"] == true {
        return Ok(None);
    }
    if sets.iter().any(|r| !observed(r)) {
        return Ok(None);
    }
    let new_desired = replicas(&sets[new])?;
    if d["spec"]["strategy"]["type"] == "Recreate" {
        for (i, r) in sets.iter().enumerate() {
            if i != new && replicas(r)? > 0 {
                return Ok(Some((i, 0)));
            }
        }
        if sets
            .iter()
            .enumerate()
            .any(|(i, r)| i != new && count(r, "replicas") > 0)
        {
            return Ok(None);
        }
        return Ok((new_desired != desired).then_some((new, desired)));
    }
    let (surge, unavailable) = bounds(d)?;
    if new_desired > desired {
        return Ok(Some((new, desired)));
    }
    let total = sets.iter().try_fold(0i64, |sum, r| {
        sum.checked_add(replicas(r)?)
            .ok_or(Error::Invalid("replica sum overflow"))
    })?;
    if new_desired < desired && total < desired + surge {
        return Ok(Some((
            new,
            new_desired + (desired - new_desired).min(desired + surge - total),
        )));
    }
    let available = sets
        .iter()
        .map(|r| count(r, "availableReplicas"))
        .sum::<i64>();
    let excess = (available - (desired - unavailable)).max(0);
    for (i, r) in sets.iter().enumerate() {
        if i == new {
            continue;
        }
        let old = replicas(r)?;
        let unhealthy = (old - count(r, "availableReplicas")).max(0);
        let remove = old.min(unhealthy + excess);
        if remove > 0 {
            return Ok(Some((i, old - remove)));
        }
    }
    Ok(None)
}
pub async fn deployment_once(client: Client, namespace: &str, name: &str) -> Result<(), Error> {
    let api = Api::<Deployment>::namespaced(client.clone(), namespace);
    let Some(d) = api.get_opt(name).await? else {
        return Ok(());
    };
    let d = serde_json::to_value(d)?;
    if !active(&d) {
        return Ok(());
    }
    let sets_api = Api::<ReplicaSet>::namespaced(client.clone(), namespace);
    let all = list(&sets_api).await?;
    let result = sync(
        &api,
        &sets_api,
        &Api::<Pod>::namespaced(client, namespace),
        &d,
        all,
    )
    .await;
    let sets: Vec<_> = list(&sets_api)
        .await?
        .into_iter()
        .filter(|r| owned(r, &d))
        .collect();
    let desired = deployment_status(&d, &sets, result.as_ref().err());
    status(&api, &d, desired).await?;
    result
}
async fn sync(
    api: &Api<Deployment>,
    sets_api: &Api<ReplicaSet>,
    pods_api: &Api<Pod>,
    d: &Value,
    all: Vec<Value>,
) -> Result<(), Error> {
    let desired = replicas(d)?;
    let minimum = minimum_ready(d)?;
    let template = &d["spec"]["template"];
    template_metadata(template)?;
    if template["metadata"]["labels"].get(HASH).is_some()
        || d["spec"]["selector"]["matchLabels"].get(HASH).is_some()
        || values(&d["spec"]["selector"]["matchExpressions"]).any(|r| r["key"] == HASH)
    {
        return Err(Error::Invalid(
            "Deployment cannot supply the controller-owned pod-template-hash label",
        ));
    }
    for mut rs in all.iter().filter(|r| active(r)).cloned() {
        let matches = matches(&d["spec"]["selector"], &rs["metadata"]["labels"]);
        let ours = owned(&rs, d);
        let orphan = !values(&rs["metadata"]["ownerReferences"]).any(|o| o["controller"] == true);
        if (ours && !matches) || (orphan && matches) {
            if !live(api, d).await? {
                return Ok(());
            }
            let mut refs = rs["metadata"]["ownerReferences"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            refs.retain(|o| o["uid"] != d["metadata"]["uid"]);
            if orphan && matches {
                refs.push(owner(d));
            }
            rs["metadata"]["ownerReferences"] = refs.into();
            replace(sets_api, &rs).await?;
            return Ok(());
        }
    }
    let mut sets: Vec<_> = all
        .into_iter()
        .filter(|r| owned(r, d) && active(r))
        .collect();
    sets.sort_by_key(|r| {
        (
            r["metadata"]["creationTimestamp"]
                .as_str()
                .unwrap_or("")
                .to_owned(),
            r["metadata"]["uid"].as_str().unwrap_or("").to_owned(),
        )
    });
    if d["spec"]["paused"] == true {
        return Ok(());
    }
    let Some(new) = current(&sets, d) else {
        if !live(api, d).await? {
            return Ok(());
        }
        let name = text(&d["metadata"], "name")?;
        let uid = text(&d["metadata"], "uid")?;
        let hash = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&json!([uid, template]))?)
        );
        let hash = &hash[..16];
        let name = format!("{}-{hash}", &name[..name.len().min(46)]);
        let mut template = template.clone();
        template["metadata"]["labels"][HASH] = hash.into();
        let mut selector = d["spec"]["selector"].clone();
        selector["matchLabels"][HASH] = hash.into();
        let revision = sets
            .iter()
            .filter_map(|r| {
                r["metadata"]["annotations"][REVISION]
                    .as_str()
                    .and_then(|s| s.parse::<i64>().ok())
            })
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(Error::Invalid("Deployment revision overflow"))?;
        let set: ReplicaSet = serde_json::from_value(
            json!({"metadata":{"name":name,"namespace":d["metadata"]["namespace"],"labels":template["metadata"]["labels"],"annotations":{REVISION:revision.to_string()},"ownerReferences":[owner(d)]},"spec":{"replicas":0,"minReadySeconds":minimum,"selector":selector,"template":template}}),
        )?;
        sets_api.create(&PostParams::default(), &set).await?;
        return Ok(());
    };
    if sets[new]["spec"]["minReadySeconds"].as_i64().unwrap_or(0) != minimum {
        if live(api, d).await? {
            sets[new]["spec"]["minReadySeconds"] = minimum.into();
            replace(sets_api, &sets[new]).await?;
        }
        return Ok(());
    }
    if let Some((index, replicas)) = plan(d, &sets, new)? {
        // Recreate waits for all old API Pods, not only delayed ReplicaSet counters.
        if d["spec"]["strategy"]["type"] == "Recreate" && index == new && replicas > 0 {
            let pods = list(pods_api).await?;
            if pods.iter().any(|p| {
                active(p)
                    && sets
                        .iter()
                        .enumerate()
                        .any(|(i, r)| i != new && owned(p, r))
            }) {
                return Ok(());
            }
        }
        if live(api, d).await? {
            sets[index]["spec"]["replicas"] = replicas.into();
            replace(sets_api, &sets[index]).await?;
        }
        return Ok(());
    }
    // Retain rollback history; remove only fully observed zero-replica old sets
    // after completion and after confirming no Pod still references that UID.
    if count(&sets[new], "availableReplicas") >= desired
        && replicas(&sets[new])? == desired
        && sets
            .iter()
            .enumerate()
            .all(|(i, r)| i == new || replicas(r).ok() == Some(0))
    {
        let keep = d["spec"]["revisionHistoryLimit"].as_u64().unwrap_or(10) as usize;
        let old: Vec<_> = sets
            .iter()
            .enumerate()
            .filter(|(i, r)| *i != new && observed(r) && count(r, "replicas") == 0)
            .map(|(_, r)| r)
            .collect();
        if old.len() > keep {
            let pods = list(pods_api).await?;
            for rs in old.iter().take(old.len() - keep) {
                if !pods.iter().any(|p| owned(p, rs)) && live(api, d).await? {
                    delete(sets_api, rs).await?;
                }
            }
        }
    }
    Ok(())
}
fn deployment_status(d: &Value, sets: &[Value], error: Option<&Error>) -> Value {
    let desired = replicas(d).unwrap_or(0);
    let old = &d["status"];
    let new = current(sets, d);
    let total = sets.iter().map(|r| count(r, "replicas")).sum::<i64>();
    let ready = sets.iter().map(|r| count(r, "readyReplicas")).sum::<i64>();
    let available = sets
        .iter()
        .map(|r| count(r, "availableReplicas"))
        .sum::<i64>();
    let updated = new.map_or(0, |i| count(&sets[i], "replicas"));
    let complete = updated == desired && total == desired && available >= desired;
    let unavailable = if d["spec"]["strategy"]["type"] == "Recreate" {
        0
    } else {
        bounds(d).map_or(0, |b| b.1)
    };
    let progressed = old["observedGeneration"] != d["metadata"]["generation"]
        || updated > old["updatedReplicas"].as_i64().unwrap_or(0)
        || available > old["availableReplicas"].as_i64().unwrap_or(0)
        || ready > old["readyReplicas"].as_i64().unwrap_or(0)
        || total - updated
            < old["replicas"].as_i64().unwrap_or(0) - old["updatedReplicas"].as_i64().unwrap_or(0);
    let previous = values(&old["conditions"]).find(|c| c["type"] == "Progressing");
    let deadline = d["spec"]["progressDeadlineSeconds"].as_i64().unwrap_or(600);
    let expired = !progressed
        && !complete
        && previous.is_some_and(|c| {
            c["reason"] == "ProgressDeadlineExceeded"
                || c["lastUpdateTime"]
                    .as_str()
                    .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                    .is_some_and(|t| now().signed_duration_since(t).num_seconds() >= deadline)
        });
    let (state, reason) = if d["spec"]["paused"] == true {
        ("Unknown", "DeploymentPaused")
    } else if complete {
        ("True", "NewReplicaSetAvailable")
    } else if expired {
        ("False", "ProgressDeadlineExceeded")
    } else {
        ("True", "ReplicaSetUpdated")
    };
    let mut conditions = vec![
        condition(
            old,
            "Available",
            if available >= desired - unavailable {
                "True"
            } else {
                "False"
            },
            if available >= desired - unavailable {
                "MinimumReplicasAvailable"
            } else {
                "MinimumReplicasUnavailable"
            },
            "observed ReplicaSet availability",
            false,
            true,
        ),
        condition(
            old,
            "Progressing",
            state,
            reason,
            "native Deployment reconciliation",
            progressed,
            true,
        ),
    ];
    if let Some(error) = error {
        conditions.push(condition(
            old,
            "ReplicaFailure",
            "True",
            "FailedManageReplicaSets",
            &failure(error),
            false,
            true,
        ));
    } else if let Some(failed) = sets
        .iter()
        .flat_map(|r| values(&r["status"]["conditions"]))
        .find(|c| c["type"] == "ReplicaFailure" && c["status"] == "True")
    {
        conditions.push(condition(
            old,
            "ReplicaFailure",
            "True",
            "FailedCreate",
            failed["message"]
                .as_str()
                .unwrap_or("ReplicaSet reconciliation failed"),
            false,
            true,
        ));
    }
    json!({"observedGeneration":d["metadata"]["generation"],"replicas":total,"updatedReplicas":updated,"readyReplicas":ready,"availableReplicas":available,"unavailableReplicas":(desired-available).max(0),"conditions":conditions})
}
#[cfg(test)]
mod tests {
    use super::*;
    fn set(desired: i64, available: i64) -> Value {
        json!({"metadata":{"generation":1},"spec":{"replicas":desired},"status":{"observedGeneration":1,"replicas":desired,"availableReplicas":available}})
    }
    fn deployment() -> Value {
        json!({"spec":{"replicas":2,"strategy":{"type":"RollingUpdate","rollingUpdate":{"maxSurge":1,"maxUnavailable":0}}}})
    }
    #[test]
    fn rollout_preserves_availability_and_waits_for_observation() {
        let d = deployment();
        assert_eq!(plan(&d, &[set(2, 2), set(0, 0)], 1).unwrap(), Some((1, 1)));
        assert_eq!(plan(&d, &[set(2, 2), set(1, 0)], 1).unwrap(), None);
        assert_eq!(plan(&d, &[set(2, 2), set(1, 1)], 1).unwrap(), Some((0, 1)));
        assert_eq!(plan(&d, &[set(1, 1), set(1, 1)], 1).unwrap(), Some((1, 2)));
        assert_eq!(plan(&d, &[set(1, 1), set(2, 1)], 1).unwrap(), None);
        assert_eq!(plan(&d, &[set(1, 1), set(2, 2)], 1).unwrap(), Some((0, 0)));
        let mut pending = set(1, 1);
        pending["metadata"]["generation"] = 2.into();
        assert_eq!(plan(&d, &[set(2, 2), pending], 1).unwrap(), None);
    }
    #[test]
    fn percentage_rounding_unhealthy_cleanup_and_zero_scale() {
        assert_eq!(fraction(&json!("25%"), 1, true).unwrap(), 1);
        assert_eq!(fraction(&json!("25%"), 1, false).unwrap(), 0);
        let mut d = deployment();
        assert_eq!(plan(&d, &[set(2, 1), set(1, 0)], 1).unwrap(), Some((0, 1)));
        d["spec"]["replicas"] = 0.into();
        assert_eq!(plan(&d, &[set(2, 2), set(0, 0)], 1).unwrap(), Some((0, 0)));
        d["spec"]["replicas"] = 1.into();
        d["spec"]["strategy"]["rollingUpdate"] = json!({"maxSurge":0,"maxUnavailable":0});
        assert!(bounds(&d).is_err());
    }
    #[test]
    fn progress_deadline_remains_failed_until_actual_progress() {
        let mut d = deployment();
        d["metadata"] = json!({"generation":1});
        d["status"] = json!({"observedGeneration":1,"replicas":0,"updatedReplicas":0,"availableReplicas":0,"readyReplicas":0,"conditions":[{"type":"Progressing","status":"False","reason":"ProgressDeadlineExceeded","lastUpdateTime":timestamp(),"lastTransitionTime":timestamp()}]});
        let status = deployment_status(&d, &[], None);
        assert!(
            values(&status["conditions"]).any(|c| c["type"] == "Progressing"
                && c["status"] == "False"
                && c["reason"] == "ProgressDeadlineExceeded")
        );
        d["metadata"]["generation"] = 2.into();
        let status = deployment_status(&d, &[], None);
        assert!(values(&status["conditions"])
            .any(|c| c["type"] == "Progressing" && c["status"] == "True"));
    }
}
