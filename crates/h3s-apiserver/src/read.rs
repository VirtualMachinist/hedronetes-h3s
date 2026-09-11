//! Reads: a single object, a paged list snapshot, or a watch stream. Node
//! grants pass a relationship guard that is re-checked as items are served.
use crate::{bad, named, nodes, object, resources::Target, Api, Failure, Result};
use axum::{
    body::{Body, Bytes},
    response::{IntoResponse, Response},
    Json,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use futures_util::StreamExt;
use h3s_storage::{EventKind, ListSelect, WatchSelect};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{collections::BTreeMap, convert::Infallible, time::Duration};

pub(crate) async fn get(api: &Api, target: &Target) -> Result<Response> {
    let obj = api
        .store
        .get(&named(target)?)
        .await?
        .ok_or_else(|| Failure::new(404, "NotFound", "object not found"))?;
    Ok(Json(object(obj)?).into_response())
}

#[derive(Serialize, Deserialize)]
struct Continue {
    revision: u64,
    key: String,
    prefix: String,
    labels: String,
    fields: String,
}
fn number(q: &BTreeMap<String, String>, name: &str) -> Result<Option<u64>> {
    q.get(name)
        .map(|v| v.parse().map_err(|_| bad("invalid numeric query value")))
        .transpose()
}

pub(crate) async fn list(
    api: &Api,
    target: &Target,
    q: &BTreeMap<String, String>,
    selection: crate::selectors::Selection,
    read_guard: Option<nodes::ReadGuard>,
) -> Result<Response> {
    let mut sel = ListSelect::new(target.prefix());
    sel.at_revision = number(q, "resourceVersion")?;
    let limit = number(q, "limit")?
        .filter(|l| *l > 0)
        .unwrap_or(256)
        .min(4096) as usize;
    let labels = q.get("labelSelector").cloned().unwrap_or_default();
    let fields = q.get("fieldSelector").cloned().unwrap_or_default();
    if let Some(token) = q.get("continue").filter(|s| !s.is_empty()) {
        let c: Continue = serde_json::from_slice(
            &URL_SAFE_NO_PAD
                .decode(token)
                .map_err(|_| bad("invalid continue token"))?,
        )?;
        if c.prefix != target.prefix() || c.labels != labels || c.fields != fields {
            return Err(bad("continue token scope or selector mismatch"));
        }
        if sel
            .at_revision
            .is_some_and(|rv| rv != 0 && rv != c.revision)
        {
            return Err(bad("continue token resourceVersion mismatch"));
        }
        sel.at_revision = Some(c.revision);
        sel.start_after = Some(crate::key(c.key)?);
    }
    let page = api.store.list(sel.clone()).await?;
    let revision = page.revision;
    sel.at_revision = Some(revision);
    let store = api.store.clone();
    let prefix = target.prefix();
    let start = format!(
        "{{\"apiVersion\":{},\"kind\":{},\"items\":[",
        json!(target.resource.api_version()),
        json!(format!("{}List", target.resource.kind))
    );
    // Continuation is known after filtering. Emit metadata after the streamed
    // items; JSON object member order is immaterial to Kubernetes clients.
    let api = api.clone();
    let stream = async_stream::try_stream! {
        yield Bytes::from(start);
        let mut page=page;let mut returned=0;let mut scanned=0;let mut next=None;
        'scan: loop {
            let count=page.items.len();
            for(i,obj)in page.items.into_iter().enumerate(){
                scanned+=1;let cursor=obj.key.clone();
                let value=object(obj).map_err(|_|std::io::Error::other("invalid stored list object"))?;
                if selection.matches(&value){
                    if let Some(guard) = &read_guard { guard.check(&api).await.map_err(|_|std::io::Error::other("node object relationship revoked or unavailable"))?; }
                    if returned>0{yield Bytes::from_static(b",");}
                    yield Bytes::from(serde_json::to_vec(&value).map_err(std::io::Error::other)?);returned+=1;
                }
                if returned>=limit||scanned>=4096{
                    if i+1<count||page.next_after.is_some(){next=Some(cursor);}break 'scan;
                }
            }
            let Some(cursor)=page.next_after else{break;};sel.start_after=Some(cursor);
            page=store.list(sel.clone()).await.map_err(|_|std::io::Error::other("registry snapshot unavailable during list"))?;
        }
        let token=next.map(|k|URL_SAFE_NO_PAD.encode(serde_json::to_vec(&Continue{revision,key:k.as_str().into(),prefix,labels,fields}).expect("continuation JSON"))).unwrap_or_default();
        yield Bytes::from(format!("],\"metadata\":{}}}",json!({"resourceVersion":revision.to_string(),"continue":token})));
    };
    let stream: std::pin::Pin<Box<dyn futures_util::Stream<Item = std::io::Result<Bytes>> + Send>> =
        Box::pin(stream);
    Ok((
        [("content-type", "application/json")],
        Body::from_stream(stream),
    )
        .into_response())
}

pub(crate) async fn watch(
    api: &Api,
    target: &Target,
    q: &BTreeMap<String, String>,
    selection: crate::selectors::Selection,
    read_guard: Option<nodes::ReadGuard>,
) -> Result<Response> {
    let initial = match q.get("sendInitialEvents").map(String::as_str) {
        None => None,
        Some("true") => Some(true),
        Some("false") => Some(false),
        _ => return Err(bad("sendInitialEvents must be true or false")),
    };
    let match_revision = q
        .get("resourceVersionMatch")
        .map(String::as_str)
        .unwrap_or("");
    if initial.is_some() {
        if match_revision != "NotOlderThan" {
            return Err(bad(
                "sendInitialEvents requires resourceVersionMatch=NotOlderThan",
            ));
        }
    } else if !match_revision.is_empty() {
        return Err(bad("watch resourceVersionMatch requires sendInitialEvents"));
    }
    if !match_revision.is_empty() && q.get("continue").is_some_and(|v| !v.is_empty()) {
        return Err(bad("watch resourceVersionMatch forbids continue"));
    }
    let requested = if q.get("resourceVersion").is_some_and(String::is_empty) {
        None
    } else {
        number(q, "resourceVersion")?
    };
    let after = match initial {
        Some(true) => {
            // NotOlderThan chooses a fresh snapshot even if the requested RV
            // was compacted. The subsequent watch anchor can only be newer.
            if let Some(requested) = requested.filter(|r| *r != 0) {
                let mut selection = ListSelect::new(target.prefix());
                selection.limit = 1;
                let current = api.store.list(selection).await?.revision;
                if requested > current {
                    return Err(h3s_storage::Error::FutureRevision { requested, current }.into());
                }
            }
            None
        }
        Some(false) if requested.is_none_or(|r| r == 0) => {
            let mut selection = ListSelect::new(target.prefix());
            selection.limit = 1;
            Some(api.store.list(selection).await?.revision)
        }
        _ => requested,
    };
    let mut stream = api
        .store
        .watch(WatchSelect::new(target.prefix(), after))
        .await?;
    let timeout = Duration::from_secs(number(q, "timeoutSeconds")?.unwrap_or(300).clamp(1, 600));
    let deadline = tokio::time::Instant::now() + timeout;
    let kind = target.resource.kind;
    let version = target.resource.api_version();
    let bookmarks = q.get("allowWatchBookmarks").is_some_and(|v| v == "true");
    let api = api.clone();
    let out = async_stream::stream! {
        let mut initial_pending = initial == Some(true);
        while let Ok(Some(event))=tokio::time::timeout_at(deadline,stream.next()).await{
            if let Some(guard) = &read_guard {
                if let Err(error) = guard.check(&api).await {
                    yield Ok::<Bytes,Infallible>(Bytes::from(format!("{}\n",json!({"type":"ERROR","object":error.value()}))));break;
                }
            }
            let wire=match event{
                Err(e)=>{yield Ok::<Bytes,Infallible>(Bytes::from(format!("{}\n",json!({"type":"ERROR","object":Failure::from(e).value()}))));break;},
                Ok(event)if event.kind==EventKind::Bookmark=>{
                    if !bookmarks && !initial_pending {continue;}
                    let mut metadata=json!({"resourceVersion":event.revision.to_string()});
                    if initial_pending {
                        metadata["annotations"]=json!({"k8s.io/initial-events-end":"true"});
                        initial_pending=false;
                    }
                    json!({"type":"BOOKMARK","object":{"apiVersion":version,"kind":kind,"metadata":metadata}})
                },
                Ok(event)=>{
                    let converted=(||->Result<_>{Ok((event.object.map(object).transpose()?,event.previous.map(object).transpose()?))})();
                    let (current,previous)=match converted{Ok(pair)=>pair,Err(e)=>{yield Ok(Bytes::from(format!("{}\n",json!({"type":"ERROR","object":e.value()}))));break;}};
                    let before=previous.as_ref().is_some_and(|v|selection.matches(v));
                    let after=event.kind!=EventKind::Deleted&&current.as_ref().is_some_and(|v|selection.matches(v));
                    let (typ,mut value)=match(before,after){
                        (false,true)=>("ADDED",current.unwrap()),(true,true)=>("MODIFIED",current.unwrap()),
                        (true,false)=>("DELETED",previous.unwrap()),(false,false)=>continue,
                    };
                    value["metadata"]["resourceVersion"]=event.revision.to_string().into();
                    json!({"type":typ,"object":value})
                }
            };
            yield Ok::<Bytes,Infallible>(Bytes::from(format!("{wire}\n")));
        }
    };
    Ok((
        [("content-type", "application/json")],
        Body::from_stream(out),
    )
        .into_response())
}
