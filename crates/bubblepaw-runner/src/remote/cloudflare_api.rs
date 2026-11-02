//! The few Cloudflare API v4 calls domain setup needs: find the zone (and its
//! account) for a hostname, create a locally managed tunnel, and point the
//! hostname at it with a proxied CNAME. The token goes only into the
//! Authorization header; errors quote Cloudflare's messages, never the token.
use super::reach::CloudflareApi;
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// The DNS target suffix of every Cloudflare Tunnel.
const TUNNEL_TARGET: &str = ".cfargotunnel.com";

pub(super) struct ApiClient {
    http: reqwest::Client,
    base: String,
    token: zeroize::Zeroizing<String>,
}

pub(super) struct Zone {
    pub id: String,
    pub account_id: String,
}

#[derive(Deserialize)]
struct Envelope {
    #[serde(default)]
    success: bool,
    #[serde(default)]
    errors: Vec<ApiMessage>,
    #[serde(default)]
    result: Value,
}

#[derive(Deserialize)]
struct ApiMessage {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    message: String,
}

impl ApiClient {
    pub(super) fn new(api: &CloudflareApi) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .build()
                .context("could not build the HTTP client")?,
            base: api.base_url.trim_end_matches('/').to_owned(),
            token: api.token.clone(),
        })
    }

    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value> {
        let mut request = self
            .http
            .request(method.clone(), format!("{}{path}", self.base))
            .bearer_auth(self.token.as_str());
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request
            .send()
            .await
            .with_context(|| format!("Cloudflare API {method} {path} failed"))?;
        let status = response.status();
        let envelope: Envelope = response.json().await.with_context(|| {
            format!("Cloudflare API {method} {path}: unreadable reply ({status})")
        })?;
        if !status.is_success() || !envelope.success {
            let messages: Vec<String> = envelope
                .errors
                .iter()
                .map(|error| format!("{} ({})", error.message, error.code))
                .collect();
            bail!(
                "Cloudflare API {method} {path} refused ({status}): {}",
                if messages.is_empty() {
                    "no reason given".to_owned()
                } else {
                    messages.join("; ")
                }
            );
        }
        Ok(envelope.result)
    }

    /// The zone the hostname belongs to: the longest suffix (two labels or
    /// more) that is a zone of this token's account.
    pub(super) async fn zone_for(&self, hostname: &str) -> Result<Zone> {
        let labels: Vec<&str> = hostname.split('.').collect();
        for start in 0..labels.len().saturating_sub(1) {
            let name = labels[start..].join(".");
            let found = self
                .call(reqwest::Method::GET, &format!("/zones?name={name}"), None)
                .await?;
            let zone = found
                .as_array()
                .into_iter()
                .flatten()
                .find(|zone| zone["name"].as_str() == Some(name.as_str()));
            if let Some(zone) = zone {
                let id = zone["id"].as_str().context("zone without an id")?;
                let account = zone["account"]["id"]
                    .as_str()
                    .context("zone without an account id")?;
                return Ok(Zone {
                    id: id.to_owned(),
                    account_id: account.to_owned(),
                });
            }
        }
        bail!("no Cloudflare zone this token can use contains {hostname}")
    }

    /// Create a locally managed tunnel with `secret`; returns its ID.
    pub(super) async fn create_tunnel(
        &self,
        account: &str,
        name: &str,
        secret: &str,
    ) -> Result<String> {
        let created = self
            .call(
                reqwest::Method::POST,
                &format!("/accounts/{account}/cfd_tunnel"),
                Some(json!({"name": name, "config_src": "local", "tunnel_secret": secret})),
            )
            .await?;
        let id = created["id"].as_str().context("tunnel without an id")?;
        uuid::Uuid::parse_str(id).context("the tunnel id is not a UUID")?;
        Ok(id.to_owned())
    }

    /// Point `hostname` at the tunnel with a proxied CNAME. An existing CNAME
    /// to another tunnel is updated; any other existing record is left alone
    /// and setup stops.
    pub(super) async fn route_dns(
        &self,
        zone: &str,
        hostname: &str,
        tunnel_id: &str,
    ) -> Result<()> {
        let target = format!("{tunnel_id}{TUNNEL_TARGET}");
        let records = self
            .call(
                reqwest::Method::GET,
                &format!("/zones/{zone}/dns_records?name={hostname}"),
                None,
            )
            .await?;
        let records: Vec<&Value> = records
            .as_array()
            .into_iter()
            .flatten()
            .filter(|record| record["name"].as_str() == Some(hostname))
            .collect();
        let record = json!({
            "type": "CNAME",
            "name": hostname,
            "content": target,
            "ttl": 1,
            "proxied": true,
        });
        match records.as_slice() {
            [] => {
                self.call(
                    reqwest::Method::POST,
                    &format!("/zones/{zone}/dns_records"),
                    Some(record),
                )
                .await?;
            }
            [existing]
                if existing["type"] == "CNAME"
                    && existing["content"]
                        .as_str()
                        .is_some_and(|content| content.ends_with(TUNNEL_TARGET)) =>
            {
                if existing["content"] != target.as_str() || existing["proxied"] != true {
                    let id = existing["id"]
                        .as_str()
                        .context("DNS record without an id")?;
                    self.call(
                        reqwest::Method::PUT,
                        &format!("/zones/{zone}/dns_records/{id}"),
                        Some(record),
                    )
                    .await?;
                }
            }
            _ => bail!(
                "{hostname} already has DNS records that do not point at a Cloudflare Tunnel; \
                 remove them or choose another remote.hostname"
            ),
        }
        Ok(())
    }
}

/// One zone as the API lists it.
pub(super) struct ZoneInfo {
    pub id: String,
    pub name: String,
    pub account_name: Option<String>,
}

fn zone_info(zone: &Value) -> Option<ZoneInfo> {
    Some(ZoneInfo {
        id: zone["id"].as_str()?.to_owned(),
        name: zone["name"].as_str()?.to_owned(),
        account_name: zone["account"]["name"].as_str().map(str::to_owned),
    })
}

impl ApiClient {
    /// The zones this token can see, up to a few hundred.
    pub(super) async fn zones(&self) -> Result<Vec<ZoneInfo>> {
        let mut zones = Vec::new();
        for page in 1..=5 {
            let found = self
                .call(
                    reqwest::Method::GET,
                    &format!("/zones?per_page=100&page={page}"),
                    None,
                )
                .await?;
            let found: Vec<ZoneInfo> = found
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(zone_info)
                .collect();
            let full = found.len() >= 100;
            zones.extend(found);
            if !full {
                break;
            }
        }
        Ok(zones)
    }

    /// One zone by ID.
    pub(super) async fn zone(&self, id: &str) -> Result<ZoneInfo> {
        let zone = self
            .call(reqwest::Method::GET, &format!("/zones/{id}"), None)
            .await?;
        zone_info(&zone).context("unreadable zone")
    }

    /// The name of the token's first account, if the token may read accounts.
    pub(super) async fn first_account_name(&self) -> Option<String> {
        let accounts = self
            .call(reqwest::Method::GET, "/accounts?per_page=1", None)
            .await
            .ok()?;
        accounts
            .as_array()?
            .first()?
            .get("name")?
            .as_str()
            .map(str::to_owned)
    }
}
