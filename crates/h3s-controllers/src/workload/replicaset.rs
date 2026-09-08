use super::*;
use futures_util::StreamExt;
use kube::runtime::{controller::Action, watcher, Controller};
use std::sync::Arc;

pub async fn run_replicaset_controller(client: Client) -> Result<(), Error> {
    Controller::new(
        Api::<ReplicaSet>::all(client.clone()),
        watcher::Config::default(),
    )
    .owns(Api::<Pod>::all(client.clone()), watcher::Config::default())
    .run(
        |rs, client: Arc<Client>| async move {
            replicaset_once(
                (*client).clone(),
                &rs.namespace()
                    .ok_or(Error::Invalid("ReplicaSet namespace missing"))?,
                &rs.name_any(),
            )
            .await?;
            Ok(Action::requeue(Duration::from_secs(2)))
        },
        |_, _: &Error, _| Action::requeue(Duration::from_secs(5)),
        Arc::new(client),
    )
    .for_each(|r| async move {
        if let Err(e) = r {
            eprintln!("ReplicaSet controller: {e}");
        }
    })
    .await;
    Err(Error::Stopped)
}
pub async fn replicaset_once(client: Client, namespace: &str, name: &str) -> Result<(), Error> {
    let api = Api::<ReplicaSet>::namespaced(client.clone(), namespace);
    let Some(rs) = api.get_opt(name).await? else {
        return Ok(());
    };
    let rs = serde_json::to_value(rs)?;
    if !active(&rs) {
        return Ok(());
    }
    let pods_api = Api::<Pod>::namespaced(client, namespace);
    let mut pods = list(&pods_api).await?;
    let result = sync(&api, &pods_api, &rs, &mut pods).await;
    let selected: Vec<_> = pods.iter().filter(|p| owned(p, &rs) && active(p)).collect();
    let minimum = minimum_ready(&rs).unwrap_or(0);
    let mut desired = json!({"observedGeneration":rs["metadata"]["generation"],"replicas":selected.len(),"fullyLabeledReplicas":selected.iter().filter(|p|rs["spec"]["template"]["metadata"]["labels"].as_object().is_none_or(|labels|labels.iter().all(|(k,v)|p["metadata"]["labels"][k]==*v))).count(),"readyReplicas":selected.iter().filter(|p|ready(p)).count(),"availableReplicas":selected.iter().filter(|p|available(p,minimum)).count()});
    if let Err(error) = &result {
        desired["conditions"] = json!([condition(
            &rs["status"],
            "ReplicaFailure",
            "True",
            "FailedManageReplicas",
            &failure(error),
            false,
            false
        )]);
    }
    status(&api, &rs, desired).await?;
    result
}
async fn sync(
    api: &Api<ReplicaSet>,
    pods_api: &Api<Pod>,
    rs: &Value,
    pods: &mut Vec<Value>,
) -> Result<(), Error> {
    let desired = replicas(rs)? as usize;
    minimum_ready(rs)?;
    let template = &rs["spec"]["template"];
    let meta = template_metadata(template)?;
    // Claim matching orphans and release our mismatches with CAS; never steal another controller's Pod.
    for p in pods.iter_mut().filter(|p| active(p)) {
        let matches = matches(&rs["spec"]["selector"], &p["metadata"]["labels"]);
        let ours = owned(p, rs);
        let orphan = !values(&p["metadata"]["ownerReferences"]).any(|o| o["controller"] == true);
        if (ours && !matches) || (orphan && matches) {
            if !live(api, rs).await? {
                return Ok(());
            }
            let mut refs = p["metadata"]["ownerReferences"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            refs.retain(|o| o["uid"] != rs["metadata"]["uid"]);
            if orphan && matches {
                refs.push(owner(rs));
            }
            p["metadata"]["ownerReferences"] = refs.into();
            replace(pods_api, p).await?;
            // Re-list next cycle before acting on adoption/release results.
            return Ok(());
        }
    }
    let mut selected: Vec<_> = pods
        .iter()
        .filter(|p| owned(p, rs) && active(p))
        .cloned()
        .collect();
    if selected.len() < desired {
        for _ in 0..(desired - selected.len()).min(BURST) {
            if !live(api, rs).await? {
                break;
            }
            let mut metadata = meta.clone();
            let name = text(&rs["metadata"], "name")?;
            metadata["generateName"] = format!("{}-", &name[..name.len().min(54)]).into();
            metadata["namespace"] = rs["metadata"]["namespace"].clone();
            metadata["ownerReferences"] = json!([owner(rs)]);
            let pod: Pod =
                serde_json::from_value(json!({"metadata":metadata,"spec":template["spec"]}))?;
            let created = pods_api.create(&PostParams::default(), &pod).await?;
            pods.push(serde_json::to_value(created)?);
        }
    } else if selected.len() > desired {
        selected.sort_by_key(|p| {
            (
                ready(p),
                p["spec"]["nodeName"]
                    .as_str()
                    .is_some_and(|s| !s.is_empty()),
                std::cmp::Reverse(
                    p["metadata"]["creationTimestamp"]
                        .as_str()
                        .unwrap_or("")
                        .to_owned(),
                ),
                p["metadata"]["uid"].as_str().unwrap_or("").to_owned(),
            )
        });
        for pod in selected.iter().take((selected.len() - desired).min(BURST)) {
            if !live(api, rs).await? {
                break;
            }
            delete(pods_api, pod).await?;
            // Status is refreshed from API below; conflicts cannot count as deletion.
        }
        *pods = list(pods_api).await?;
    }
    Ok(())
}
