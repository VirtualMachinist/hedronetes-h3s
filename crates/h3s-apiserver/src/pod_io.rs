//! Pod log and exec subresources. The API resolves the assigned node and
//! container from stored status, then calls the kubelet over the supervisor
//! tunnel with the cluster kubelet client — never the caller's certificate.
use crate::{bad, key, kubelet, object, Api, Failure, Result, Target};
use axum::{
    body::{to_bytes, Body},
    extract::{
        ws::{close_code, CloseFrame, Message, WebSocketUpgrade},
        FromRequest,
    },
    http::Request,
};
use futures_util::SinkExt;
use serde_json::{json, Value};
use std::collections::BTreeMap;

const LOG_BYTES: usize = 2 * 1024 * 1024;

pub async fn logs(
    api: &Api,
    target: &Target,
    q: &BTreeMap<String, String>,
) -> Result<axum::response::Response> {
    for key in q.keys() {
        if !matches!(
            key.as_str(),
            "container" | "follow" | "previous" | "timestamps" | "tailLines" | "timeout" | "pretty"
        ) {
            return Err(bad("unsupported log query parameter"));
        }
    }
    if flag(q, "follow")? {
        return Err(Failure::new(
            400,
            "BadRequest",
            "log follow is not implemented",
        ));
    }
    let pod = load_pod(api, target).await?;
    let previous = flag(q, "previous")?;
    let timestamps = flag(q, "timestamps")?;
    let tail_lines = match q.get("tailLines") {
        None => None,
        Some(v) => Some(v.parse::<u32>().map_err(|_| bad("invalid tailLines"))?),
    };
    let (container, mut attempt, _) =
        resolve_container(&pod, q.get("container").map(String::as_str))?;
    if previous {
        if attempt == 0 {
            return Err(bad("previous container log is not available"));
        }
        attempt -= 1;
    }
    let node = assigned_node(&pod)?;
    let uid = pod["metadata"]["uid"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Failure::new(500, "InternalError", "Pod uid is missing"))?
        .to_owned();
    let mut query = vec![
        ("uid".to_string(), uid),
        ("container".to_string(), container),
        ("attempt".to_string(), attempt.to_string()),
    ];
    if timestamps {
        query.push(("timestamps".to_string(), "true".into()));
    }
    if let Some(n) = tail_lines {
        query.push(("tailLines".to_string(), n.to_string()));
    }
    kubelet::forward(api, node, "GET", "containerLogs", &query, LOG_BYTES).await
}

pub async fn exec(
    api: &Api,
    target: &Target,
    q: &BTreeMap<String, String>,
    commands: &[String],
    request: Request<Body>,
) -> Result<axum::response::Response> {
    for key in q.keys() {
        if !matches!(
            key.as_str(),
            "container"
                | "stdin"
                | "stdout"
                | "stderr"
                | "tty"
                | "timeoutSeconds"
                | "timeout"
                | "pretty"
        ) {
            return Err(bad("unsupported exec query parameter"));
        }
    }
    if flag(q, "stdin")? || flag(q, "tty")? {
        return Err(Failure::new(
            400,
            "BadRequest",
            "streaming exec (stdin/tty) is not implemented",
        ));
    }
    if commands.is_empty() || commands.iter().any(|c| c.is_empty()) {
        return Err(bad("exec requires a command"));
    }
    let pod = load_pod(api, target).await?;
    let (_, _, container_id) = resolve_container(&pod, q.get("container").map(String::as_str))?;
    if container_id.is_empty() {
        return Err(Failure::new(409, "Conflict", "container is not running"));
    }
    let node = assigned_node(&pod)?.to_owned();
    let timeout = match q.get("timeoutSeconds") {
        None => 10u32,
        Some(v) => v
            .parse::<u32>()
            .ok()
            .filter(|n| (1..=30).contains(n))
            .ok_or_else(|| bad("invalid timeoutSeconds"))?,
    };
    let mut query = vec![
        ("containerId".to_string(), container_id),
        ("timeoutSeconds".to_string(), timeout.to_string()),
    ];
    for command in commands {
        query.push(("command".to_string(), command.clone()));
    }
    let upgrade = request
        .headers()
        .get("upgrade")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
    if upgrade {
        return websocket_exec(api.clone(), node, query, request).await;
    }
    kubelet::forward(api, &node, "POST", "exec", &query, LOG_BYTES).await
}

async fn websocket_exec(
    api: Api,
    node: String,
    query: Vec<(String, String)>,
    request: Request<Body>,
) -> Result<axum::response::Response> {
    let ws = WebSocketUpgrade::from_request(request, &())
        .await
        .map_err(|_| bad("websocket upgrade failed"))?;
    Ok(ws
        .protocols([
            "v5.channel.k8s.io",
            "v4.channel.k8s.io",
            "v3.channel.k8s.io",
        ])
        .on_upgrade(move |mut socket| async move {
            let frame = match kubelet::forward(&api, &node, "POST", "exec", &query, LOG_BYTES).await
            {
                Ok(response) => match exec_frames(response).await {
                    Ok(frames) => frames,
                    Err(error) => vec![status_frame(&error.value())],
                },
                Err(error) => vec![status_frame(&error.value())],
            };
            for message in frame {
                if socket.send(message).await.is_err() {
                    break;
                }
            }
            // kubectl's error stream treats a 1005 (no status) close as failure
            // even when stdout already arrived. Close normally after the v4
            // status frame.
            let _ = socket.send(exec_websocket_close()).await;
            let _ = socket.close().await;
        }))
}

fn exec_websocket_close() -> Message {
    Message::Close(Some(CloseFrame {
        code: close_code::NORMAL,
        reason: "".into(),
    }))
}

async fn exec_frames(response: axum::response::Response) -> Result<Vec<Message>> {
    let (parts, body) = response.into_parts();
    let bytes = to_bytes(body, LOG_BYTES)
        .await
        .map_err(|_| Failure::new(502, "BadGateway", "kubelet exec response failed"))?;
    if !parts.status.is_success() {
        let value: Value = serde_json::from_slice(&bytes).unwrap_or_else(|_| {
            json!({"apiVersion":"v1","kind":"Status","status":"Failure","message":String::from_utf8_lossy(&bytes),"code":parts.status.as_u16()})
        });
        return Ok(vec![status_frame(&value)]);
    }
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|_| Failure::new(502, "BadGateway", "kubelet exec returned invalid JSON"))?;
    let mut frames = Vec::new();
    if let Some(stdout) = value["stdout"].as_str().filter(|s| !s.is_empty()) {
        frames.push(channel(1, stdout.as_bytes()));
    }
    if let Some(stderr) = value["stderr"].as_str().filter(|s| !s.is_empty()) {
        frames.push(channel(2, stderr.as_bytes()));
    }
    let exit = value["exitCode"].as_i64().unwrap_or(0);
    if exit == 0 {
        frames.push(status_frame(
            &json!({"apiVersion":"v1","kind":"Status","status":"Success","metadata":{}}),
        ));
    } else {
        frames.push(status_frame(&json!({
            "apiVersion":"v1",
            "kind":"Status",
            "status":"Failure",
            "reason":"NonZeroExitCode",
            "details":{"causes":[{"reason":"ExitCode","message":exit.to_string()}]}
        })));
    }
    Ok(frames)
}
fn channel(id: u8, data: &[u8]) -> Message {
    let mut buf = Vec::with_capacity(1 + data.len());
    buf.push(id);
    buf.extend_from_slice(data);
    Message::Binary(buf.into())
}
fn status_frame(value: &Value) -> Message {
    channel(3, value.to_string().as_bytes())
}

async fn load_pod(api: &Api, target: &Target) -> Result<Value> {
    let ns = target
        .namespace
        .as_ref()
        .ok_or_else(|| bad("namespaced writes and named reads require a namespace"))?;
    let name = target
        .name
        .as_ref()
        .ok_or_else(|| bad("Pod name is required"))?;
    let stored = api
        .store
        .get(&key(format!("/registry/pods/{ns}/{name}"))?)
        .await?
        .ok_or_else(|| Failure::new(404, "NotFound", "object not found"))?;
    object(stored)
}
fn assigned_node(pod: &Value) -> Result<&str> {
    pod["spec"]["nodeName"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| bad("Pod is not assigned to a node"))
}
fn resolve_container(pod: &Value, requested: Option<&str>) -> Result<(String, u32, String)> {
    let spec = pod["spec"]["containers"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if spec.is_empty() {
        return Err(bad("Pod has no containers"));
    }
    let name = match requested {
        Some(name) => {
            if !spec.iter().any(|c| c["name"].as_str() == Some(name)) {
                return Err(bad("container not found in Pod spec"));
            }
            name.to_owned()
        }
        None if spec.len() == 1 => spec[0]["name"]
            .as_str()
            .ok_or_else(|| bad("container name is required"))?
            .to_owned(),
        None => {
            return Err(bad(
                "a container name is required when a Pod has more than one container",
            ))
        }
    };
    let status = pod["status"]["containerStatuses"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|c| c["name"].as_str() == Some(name.as_str()));
    let attempt = status.and_then(|c| c["restartCount"].as_u64()).unwrap_or(0) as u32;
    let raw_id = status.and_then(|c| c["containerID"].as_str()).unwrap_or("");
    let id = raw_id
        .strip_prefix("containerd://")
        .unwrap_or(raw_id)
        .to_owned();
    Ok((name, attempt, id))
}
fn flag(q: &BTreeMap<String, String>, name: &str) -> Result<bool> {
    match q.get(name).map(String::as_str) {
        None => Ok(false),
        Some("true") | Some("1") => Ok(true),
        Some("false") | Some("0") => Ok(false),
        Some(_) => Err(bad(&format!("invalid {name} parameter"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_websocket_closes_with_normal_code() {
        match exec_websocket_close() {
            Message::Close(Some(frame)) => {
                assert_eq!(frame.code, close_code::NORMAL);
                assert!(frame.reason.is_empty());
            }
            other => panic!("expected Close 1000, got {other:?}"),
        }
    }
}
