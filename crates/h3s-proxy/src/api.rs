use crate::{Error, Result};
use reqwest::{Client, Url};
use serde_json::Value;
use std::{collections::BTreeSet, time::Duration};
const MAX_BYTES: usize = 16 * 1024 * 1024;
const MAX_ITEMS: usize = 8192;
pub(crate) struct List {
    pub items: Vec<Value>,
    pub revision: String,
}
pub(crate) struct Snapshot {
    pub services: List,
    pub slices: List,
    pub nodes: List,
}
fn origin(endpoint: &Url, path: &str) -> Result<Url> {
    if endpoint.scheme() != "https"
        || endpoint.host_str().is_none()
        || endpoint.path() != "/"
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
    {
        return Err(Error::Invalid("API endpoint must be an HTTPS origin"));
    }
    endpoint
        .join(path)
        .map_err(|_| Error::Invalid("invalid API path"))
}
async fn list(client: &Client, endpoint: &Url, path: &str) -> Result<List> {
    let mut pages = Pages::default();
    let mut bytes = 0;
    loop {
        let mut url = origin(endpoint, path)?;
        url.query_pairs_mut().append_pair("limit", "256");
        if !pages.continuation.is_empty() {
            url.query_pairs_mut()
                .append_pair("continue", &pages.continuation);
        }
        let mut response = client
            .get(url)
            .timeout(Duration::from_secs(10))
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(Error::Status(response.status().as_u16()));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            bytes += chunk.len();
            if bytes > MAX_BYTES {
                return Err(Error::Invalid("API list exceeds byte limit"));
            }
            body.extend_from_slice(&chunk);
        }
        let value: Value = serde_json::from_slice(&body)?;
        if pages.push(value)? {
            return Ok(List {
                items: pages.items,
                revision: pages.revision.unwrap(),
            });
        }
    }
}
#[derive(Default)]
struct Pages {
    items: Vec<Value>,
    revision: Option<String>,
    continuation: String,
    cursors: BTreeSet<String>,
}
impl Pages {
    /// No partial list is exposed to the caller on any pagination failure.
    fn push(&mut self, value: Value) -> Result<bool> {
        let rv = value["metadata"]["resourceVersion"]
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= 128)
            .ok_or(Error::Invalid("list resourceVersion missing"))?;
        if self.revision.as_deref().is_some_and(|old| old != rv) {
            return Err(Error::Invalid("list pagination changed revision"));
        }
        self.revision = Some(rv.to_owned());
        let page = value["items"]
            .as_array()
            .ok_or(Error::Invalid("list items missing"))?;
        if self.items.len() + page.len() > MAX_ITEMS {
            return Err(Error::Invalid("API list exceeds item limit"));
        }
        self.items.extend(page.iter().cloned());
        self.continuation = match &value["metadata"]["continue"] {
            Value::Null => String::new(),
            Value::String(s) => s.clone(),
            _ => return Err(Error::Invalid("invalid list continuation type")),
        };
        if self.continuation.is_empty() {
            return Ok(true);
        }
        if self.continuation.len() > 4096
            || !self.cursors.insert(self.continuation.clone())
            || self.cursors.len() > 64
        {
            return Err(Error::Invalid("invalid or excessive list continuation"));
        }
        Ok(false)
    }
}

pub(crate) async fn snapshot(client: &Client, endpoint: &Url) -> Result<Snapshot> {
    tokio::time::timeout(Duration::from_secs(30), async {
        let (services, slices, nodes) = tokio::try_join!(
            list(client, endpoint, "api/v1/services"),
            list(client, endpoint, "apis/discovery.k8s.io/v1/endpointslices"),
            list(client, endpoint, "api/v1/nodes")
        )?;
        Ok(Snapshot {
            services,
            slices,
            nodes,
        })
    })
    .await
    .map_err(|_| Error::Invalid("snapshot timed out"))?
}
pub(crate) async fn changed(
    client: &Client,
    endpoint: &Url,
    path: &str,
    revision: &str,
) -> Result<()> {
    let mut url = origin(endpoint, path)?;
    url.query_pairs_mut()
        .append_pair("watch", "true")
        .append_pair("resourceVersion", revision)
        .append_pair("timeoutSeconds", "25");
    let mut response = client
        .get(url)
        .timeout(Duration::from_secs(30))
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(Error::Status(response.status().as_u16()));
    }
    // Bytes (including ERROR/410) trigger a fresh LIST, never direct mutation.
    let _ = response.chunk().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn pagination_requires_complete_consistent_bounded_snapshots() {
        let mut pages = Pages::default();
        assert!(!pages
            .push(
                json!({"metadata":{"resourceVersion":"7","continue":"next/+?"},"items":[{"id":1}]})
            )
            .unwrap());
        assert!(pages
            .push(json!({"metadata":{"resourceVersion":"7"},"items":[{"id":2}]}))
            .unwrap());
        assert_eq!(pages.items, vec![json!({"id":1}), json!({"id":2})]);
        assert_eq!(pages.revision.as_deref(), Some("7"));
        for second in [
            json!({"metadata":{"resourceVersion":"8"},"items":[]}),
            json!({"metadata":{"resourceVersion":"7","continue":"again"},"items":[]}),
            json!({"metadata":{"resourceVersion":"7","continue":false},"items":[]}),
            json!({"metadata":{"resourceVersion":"7","continue":"x".repeat(4097)},"items":[]}),
            json!({"metadata":{"resourceVersion":"7"}}),
            json!({"metadata":{},"items":[]}),
        ] {
            let mut pages = Pages::default();
            assert!(!pages
                .push(json!({"metadata":{"resourceVersion":"7","continue":"again"},"items":[]}))
                .unwrap());
            assert!(pages.push(second).is_err());
        }
        assert!(Pages::default()
            .push(json!({"metadata":{"resourceVersion":"7"},"items":vec![Value::Null;MAX_ITEMS+1]}))
            .is_err());
        let mut pages = Pages::default();
        for n in 0..64 {
            assert!(!pages.push(json!({"metadata":{"resourceVersion":"7","continue":format!("page-{n}")},"items":[]})).unwrap());
        }
        assert!(pages
            .push(json!({"metadata":{"resourceVersion":"7","continue":"page-65"},"items":[]}))
            .is_err());
    }
    #[test]
    fn origin_and_continuation_cannot_redirect_discovery() {
        for endpoint in [
            "http://localhost/",
            "https://user:pass@localhost/",
            "https://localhost/path",
            "https://localhost/?q=x",
            "https://localhost/#fragment",
        ] {
            assert!(origin(&Url::parse(endpoint).unwrap(), "api/v1/services").is_err());
        }
        let mut url = origin(
            &Url::parse("https://localhost:6443/").unwrap(),
            "api/v1/services",
        )
        .unwrap();
        let cursor = "https://elsewhere/?watch=true&limit=999#x";
        url.query_pairs_mut().append_pair("continue", cursor);
        assert_eq!(url.host_str(), Some("localhost"));
        assert_eq!(url.path(), "/api/v1/services");
        assert_eq!(
            url.query_pairs().collect::<Vec<_>>(),
            vec![("continue".into(), cursor.into())]
        );
    }
}
