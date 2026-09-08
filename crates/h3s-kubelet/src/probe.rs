use crate::{invalid, pod, Result};
use h3s_cri::{v1::ExecSyncRequest, Cri};
use serde_json::Value;
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};
#[derive(Default)]
pub struct State {
    entries: HashMap<String, Entry>,
}
struct Entry {
    checked: Instant,
    success: u32,
    failure: u32,
    ready: bool,
}
impl State {
    pub async fn ready(
        &mut self,
        cri: &Cri,
        id: &str,
        c: &Value,
        ip: &str,
        started: i64,
    ) -> Result<bool> {
        let p = &c["readinessProbe"];
        if p.is_null() {
            return Ok(true);
        }
        let age = (time::OffsetDateTime::now_utc().unix_timestamp_nanos() - i128::from(started))
            / 1_000_000_000;
        if age < i128::from(p["initialDelaySeconds"].as_u64().unwrap_or(0)) {
            return Ok(false);
        }
        let period = Duration::from_secs(p["periodSeconds"].as_u64().unwrap_or(10).max(1));
        if let Some(e) = self.entries.get(id) {
            if e.checked.elapsed() < period {
                return Ok(e.ready);
            }
        }
        let timeout = Duration::from_secs(p["timeoutSeconds"].as_u64().unwrap_or(1).clamp(1, 60));
        let passed = tokio::time::timeout(timeout, check(cri, id, c, ip))
            .await
            .is_ok_and(|r| r.unwrap_or(false));
        let e = self.entries.entry(id.into()).or_insert(Entry {
            checked: Instant::now(),
            success: 0,
            failure: 0,
            ready: false,
        });
        e.checked = Instant::now();
        if passed {
            e.success = e.success.saturating_add(1);
            e.failure = 0;
            if e.success
                >= p["successThreshold"]
                    .as_u64()
                    .unwrap_or(1)
                    .clamp(1, u32::MAX as u64) as u32
            {
                e.ready = true;
            }
        } else {
            e.failure = e.failure.saturating_add(1);
            e.success = 0;
            if e.failure
                >= p["failureThreshold"]
                    .as_u64()
                    .unwrap_or(3)
                    .clamp(1, u32::MAX as u64) as u32
            {
                e.ready = false;
            }
        }
        Ok(e.ready)
    }
    pub fn retain(&mut self, ids: &std::collections::HashSet<String>) {
        self.entries.retain(|id, _| ids.contains(id));
    }
}
fn port(v: &Value, c: &Value) -> Result<u16> {
    let number = v
        .as_u64()
        .or_else(|| {
            v.as_str().and_then(|name| {
                c["ports"].as_array()?.iter().find(|p| p["name"] == name)?["containerPort"].as_u64()
            })
        })
        .ok_or_else(|| invalid("invalid probe port"))?;
    u16::try_from(number)
        .ok()
        .filter(|p| *p != 0)
        .ok_or_else(|| invalid("invalid probe port"))
}
async fn check(cri: &Cri, id: &str, c: &Value, ip: &str) -> Result<bool> {
    let p = &c["readinessProbe"];
    if !p["exec"].is_null() {
        let r = cri
            .runtime()
            .exec_sync(ExecSyncRequest {
                container_id: id.into(),
                cmd: pod::strings(&p["exec"]["command"])?,
                timeout: p["timeoutSeconds"].as_i64().unwrap_or(1).clamp(1, 60),
            })
            .await
            .map_err(h3s_cri::Error::from)?
            .into_inner();
        return Ok(r.exit_code == 0);
    }
    let address: std::net::IpAddr = ip
        .parse()
        .map_err(|_| invalid("missing Pod IP for readiness probe"))?;
    if address.is_loopback() || address.is_unspecified() || address.is_multicast() {
        return Err(invalid("invalid Pod IP for readiness probe"));
    }
    if !p["tcpSocket"].is_null() {
        return Ok(
            tokio::net::TcpStream::connect((address, port(&p["tcpSocket"]["port"], c)?))
                .await
                .is_ok(),
        );
    }
    let endpoint = std::net::SocketAddr::new(address, port(&p["httpGet"]["port"], c)?);
    let mut url = reqwest::Url::parse(&format!("http://{endpoint}/")).expect("IP URL");
    let path = p["httpGet"]["path"].as_str().unwrap_or("/");
    if let Some((path, query)) = path.split_once('?') {
        url.set_path(path);
        url.set_query(Some(query));
    } else {
        url.set_path(path);
    }
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(60))
        .build()?;
    // No caller-selected host, redirects, credentials or output retention.
    Ok(client
        .get(url)
        .header("user-agent", "h3s-probe")
        .send()
        .await
        .is_ok_and(|r| r.status().as_u16() >= 200 && r.status().as_u16() < 400))
}
