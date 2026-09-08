use crate::{Error, Result, MAX_RULESET, TABLE};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    time::Duration,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

pub(crate) struct Backend {
    binary: PathBuf,
    owner: String,
    applied: Option<String>,
    observed: Option<Value>,
}
impl Backend {
    pub fn new(binary: &Path, owner: &str) -> Result<Self> {
        if !binary.is_absolute()
            || !binary.metadata()?.is_file()
            || owner.len() != 64
            || !owner.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(Error::Invalid(
                "require an absolute nft executable and ownership digest",
            ));
        }
        Ok(Self {
            binary: binary.into(),
            owner: format!("hedronetes.io/service-proxy/v1:{owner}"),
            applied: None,
            observed: None,
        })
    }
    async fn command(&self, args: &[&str], input: Option<&str>) -> Result<Vec<u8>> {
        let mut child = tokio::process::Command::new(&self.binary)
            .args(args)
            .env("LC_ALL", "C")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let mut stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let work = async {
            let write = async {
                if let Some(input) = input {
                    stdin.write_all(input.as_bytes()).await?;
                }
                drop(stdin);
                Ok::<(), Error>(())
            };
            let wait = async { child.wait().await.map_err(Error::from) };
            let (_, output, errors, status) = tokio::try_join!(
                write,
                bounded(stdout, 4 * MAX_RULESET),
                bounded(stderr, 65536),
                wait
            )?;
            check_status(status, &errors)?;
            Ok(output)
        };
        tokio::time::timeout(Duration::from_secs(10), work)
            .await
            .map_err(|_| Error::Invalid("nft helper timed out"))?
    }
    async fn inspect(&self) -> Result<Option<Value>> {
        let list: Value =
            serde_json::from_slice(&self.command(&["--json", "list", "tables"], None).await?)?;
        let objects = list["nftables"]
            .as_array()
            .ok_or(Error::Invalid("invalid nft table inventory"))?;
        if !objects
            .iter()
            .any(|o| o["table"]["family"] == "ip" && o["table"]["name"] == TABLE)
        {
            return Ok(None);
        }
        let table: Value = serde_json::from_slice(
            &self
                .command(
                    &["--json", "--stateless", "list", "table", "ip", TABLE],
                    None,
                )
                .await?,
        )?;
        let objects = table["nftables"]
            .as_array()
            .ok_or(Error::Invalid("invalid nft table state"))?;
        if !objects.iter().any(|o| {
            o["table"]["family"] == "ip"
                && o["table"]["name"] == TABLE
                && o["table"]["comment"] == self.owner
        }) {
            return Err(Error::Invalid(
                "refusing to modify a foreign nftables table",
            ));
        }
        Ok(Some(normalize(table)))
    }
    pub async fn reconcile(&mut self, rules: &str) -> Result<bool> {
        if rules.len() > MAX_RULESET {
            return Err(Error::Invalid("ruleset exceeds byte limit"));
        }
        let observed = self.inspect().await?;
        if self.applied.as_deref() == Some(rules) && observed.is_some() && self.observed == observed
        {
            return Ok(false);
        }
        // The complete transaction either replaces this owned table or leaves
        // it untouched. No global flush, shell, helper include or packet mark.
        self.command(&["--file", "-"], Some(rules)).await?;
        self.observed = self.inspect().await?;
        if self.observed.is_none() {
            return Err(Error::Invalid("applied proxy table is missing"));
        }
        self.applied = Some(rules.to_owned());
        Ok(true)
    }
}
async fn bounded<R: AsyncRead + Unpin>(reader: R, limit: usize) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    reader.take(limit as u64 + 1).read_to_end(&mut data).await?;
    if data.len() > limit {
        return Err(Error::Invalid("nft output exceeds limit"));
    }
    Ok(data)
}
fn check_status(status: ExitStatus, stderr: &[u8]) -> Result<()> {
    if status.success() {
        Ok(())
    } else {
        Err(Error::Nft(
            String::from_utf8_lossy(stderr).chars().take(1024).collect(),
        ))
    }
}
fn normalize(mut value: Value) -> Value {
    match &mut value {
        Value::Object(map) => {
            for key in ["handle", "packets", "bytes", "metainfo"] {
                map.remove(key);
            }
            for item in map.values_mut() {
                *item = normalize(item.take());
            }
        }
        Value::Array(items) => {
            for item in items {
                *item = normalize(item.take());
            }
        }
        _ => {}
    }
    value
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn kernel_comparison_ignores_counters_and_handles_but_detects_rule_drift() {
        let a = json!({"nftables":[{"metainfo":{"version":"1"}},{"rule":{"handle":1,"expr":[{"counter":{"packets":3,"bytes":50}},{"dnat":{"addr":"10.42.0.2","port":80}}]}}]});
        let mut b = a.clone();
        b["nftables"][1]["rule"]["handle"] = json!(7);
        b["nftables"][1]["rule"]["expr"][0]["counter"]["bytes"] = json!(900);
        assert_eq!(normalize(a.clone()), normalize(b.clone()));
        b["nftables"][1]["rule"]["expr"][1]["dnat"]["addr"] = json!("10.42.0.3");
        assert_ne!(normalize(a), normalize(b));
    }
    #[tokio::test]
    async fn bounded_helper_output_fails_closed() {
        assert!(bounded(&b"oversized"[..], 4).await.is_err());
        assert_eq!(bounded(&b"ok"[..], 4).await.unwrap(), b"ok");
    }
}
