//! RFC 9728 protected-resource metadata and RFC 8414 / OpenID Connect
//! authorization-server metadata, in the order the MCP authorization spec lists.
use super::{endpoint, get_json};
use anyhow::{Result, anyhow, bail, ensure};
use reqwest::{Client, Url};
use serde_json::Value;

/// The useful parameters of a `WWW-Authenticate: Bearer ...` challenge.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Challenge {
    pub resource_metadata: Option<String>,
    pub scope: Option<String>,
}

impl Challenge {
    /// Parses the Bearer challenge's auth-params; other schemes are ignored.
    pub(crate) fn parse(header: &str) -> Option<Self> {
        let start = header.to_ascii_lowercase().find("bearer")?;
        let mut rest = &header[start + "bearer".len()..];
        let mut challenge = Self::default();
        loop {
            rest = rest.trim_start_matches([' ', ',', '\t']);
            let Some(equals) = rest.find('=') else { break };
            let name = rest[..equals].trim().to_ascii_lowercase();
            if name.contains(' ') {
                break; // the next challenge's scheme
            }
            rest = &rest[equals + 1..];
            let value = if let Some(quoted) = rest.strip_prefix('"') {
                let mut value = String::new();
                let mut chars = quoted.char_indices();
                let mut end = quoted.len();
                while let Some((index, char)) = chars.next() {
                    match char {
                        '\\' => {
                            if let Some((_, next)) = chars.next() {
                                value.push(next);
                            }
                        }
                        '"' => {
                            end = index + 1;
                            break;
                        }
                        _ => value.push(char),
                    }
                }
                rest = &quoted[end.min(quoted.len())..];
                value
            } else {
                let end = rest.find(',').unwrap_or(rest.len());
                let value = rest[..end].trim().to_owned();
                rest = &rest[end..];
                value
            };
            match name.as_str() {
                "resource_metadata" => challenge.resource_metadata = Some(value),
                "scope" => challenge.scope = Some(value),
                _ => {}
            }
        }
        Some(challenge)
    }
}

/// Everything a sign-in needs, already validated.
#[derive(Clone, Debug)]
pub(crate) struct Metadata {
    pub resource: String,
    pub issuer: String,
    pub authorization_endpoint: Url,
    pub token_endpoint: Url,
    pub registration_endpoint: Option<Url>,
    pub scope: Option<String>,
}

pub(crate) async fn discover(
    client: &Client,
    server: &Url,
    challenge: Option<&Challenge>,
) -> Result<Metadata> {
    let (resource, issuer, supported_scopes) =
        protected_resource(client, server, challenge).await?;
    let issuer_url = endpoint(&issuer, "authorization server")?;
    let mut found = None;
    for url in authorization_server_urls(&issuer_url) {
        if let Some(document) = get_json(client, &url).await? {
            found = Some(document);
            break;
        }
    }
    let document =
        found.ok_or_else(|| anyhow!("MCP OAuth authorization server metadata was not found"))?;
    ensure!(
        document["issuer"].as_str() == Some(issuer.as_str()),
        "MCP OAuth authorization server metadata names a different issuer"
    );
    let methods = document["code_challenge_methods_supported"].as_array();
    ensure!(
        methods.is_some_and(|methods| methods.iter().any(|method| method == "S256")),
        "MCP OAuth authorization server does not support PKCE S256"
    );
    let url = |field: &str| -> Result<Url> {
        let text = document[field]
            .as_str()
            .ok_or_else(|| anyhow!("MCP OAuth authorization server metadata lacks {field}"))?;
        endpoint(text, field)
    };
    let registration_endpoint = match document.get("registration_endpoint") {
        Some(Value::String(text)) => Some(endpoint(text, "registration_endpoint")?),
        _ => None,
    };
    let scope = challenge
        .and_then(|challenge| challenge.scope.clone())
        .or(supported_scopes)
        .filter(|scope| !scope.trim().is_empty());
    Ok(Metadata {
        resource,
        issuer,
        authorization_endpoint: url("authorization_endpoint")?,
        token_endpoint: url("token_endpoint")?,
        registration_endpoint,
        scope,
    })
}

/// Returns (resource, first authorization server, space-joined scopes).
async fn protected_resource(
    client: &Client,
    server: &Url,
    challenge: Option<&Challenge>,
) -> Result<(String, String, Option<String>)> {
    let candidates = match challenge.and_then(|c| c.resource_metadata.as_deref()) {
        Some(text) => vec![endpoint(text, "resource metadata URL")?],
        None => well_known(server, "oauth-protected-resource", true),
    };
    let mut document = None;
    for url in candidates {
        if let Some(found) = get_json(client, &url).await? {
            document = Some(found);
            break;
        }
    }
    let Some(document) = document else {
        bail!("MCP OAuth protected resource metadata was not found");
    };
    let resource = document["resource"]
        .as_str()
        .ok_or_else(|| anyhow!("MCP OAuth protected resource metadata lacks resource"))?;
    ensure!(
        resource_covers(resource, server),
        "MCP OAuth protected resource metadata names a different resource"
    );
    let issuer = document["authorization_servers"]
        .as_array()
        .and_then(|servers| servers.first())
        .and_then(Value::as_str)
        .ok_or_else(|| {
            anyhow!("MCP OAuth protected resource metadata names no authorization server")
        })?;
    let scopes = document["scopes_supported"].as_array().map(|scopes| {
        scopes
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" ")
    });
    Ok((resource.to_owned(), issuer.to_owned(), scopes))
}

/// The resource must be the server URL itself or a path prefix on the same origin.
fn resource_covers(resource: &str, server: &Url) -> bool {
    let Ok(resource) = Url::parse(resource) else {
        return false;
    };
    let path = resource.path().trim_end_matches('/');
    resource.origin() == server.origin()
        && resource.query().is_none()
        && resource.fragment().is_none()
        && (server.path().trim_end_matches('/') == path
            || server.path().starts_with(&format!("{path}/")))
}

/// `/.well-known/<name><path>` first when the URL has a path, then the root form.
fn well_known(url: &Url, name: &str, with_root: bool) -> Vec<Url> {
    let path = url.path().trim_end_matches('/');
    let mut urls = Vec::new();
    let mut at = |text: String| {
        let mut url = url.clone();
        url.set_path(&text);
        url.set_query(None);
        urls.push(url);
    };
    if !path.is_empty() {
        at(format!("/.well-known/{name}{path}"));
    }
    if path.is_empty() || with_root {
        at(format!("/.well-known/{name}"));
    }
    urls
}

/// RFC 8414 first, then OpenID Connect discovery with path insertion and with
/// the path-appended form, as the MCP authorization spec requires.
fn authorization_server_urls(issuer: &Url) -> Vec<Url> {
    let path = issuer.path().trim_end_matches('/');
    let mut urls = well_known(issuer, "oauth-authorization-server", false);
    urls.extend(well_known(issuer, "openid-configuration", false));
    if !path.is_empty() {
        let mut url = issuer.clone();
        url.set_path(&format!("{path}/.well-known/openid-configuration"));
        urls.push(url);
    }
    urls
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenges_and_well_known_urls() {
        let challenge = Challenge::parse(
            r#"Bearer error="invalid_token", resource_metadata="https://a.test/.well-known/oauth-protected-resource/mcp", scope="read write""#,
        )
        .unwrap();
        assert_eq!(
            challenge.resource_metadata.as_deref(),
            Some("https://a.test/.well-known/oauth-protected-resource/mcp")
        );
        assert_eq!(challenge.scope.as_deref(), Some("read write"));
        assert!(Challenge::parse("Basic realm=x").is_none());

        let server = Url::parse("https://a.test/mcp").unwrap();
        let names: Vec<_> = well_known(&server, "oauth-protected-resource", true)
            .iter()
            .map(Url::path)
            .map(str::to_owned)
            .collect();
        assert_eq!(
            names,
            [
                "/.well-known/oauth-protected-resource/mcp",
                "/.well-known/oauth-protected-resource"
            ]
        );
        let issuer = Url::parse("https://auth.test/tenant").unwrap();
        let names: Vec<_> = authorization_server_urls(&issuer)
            .iter()
            .map(|url| url.path().to_owned())
            .collect();
        assert_eq!(
            names,
            [
                "/.well-known/oauth-authorization-server/tenant",
                "/.well-known/openid-configuration/tenant",
                "/tenant/.well-known/openid-configuration"
            ]
        );
        assert!(resource_covers("https://a.test/mcp", &server));
        assert!(resource_covers("https://a.test/", &server));
        assert!(!resource_covers("https://a.test/other", &server));
        assert!(!resource_covers("https://b.test/mcp", &server));
    }
}
