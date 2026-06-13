use crate::context::{Approval, ToolCx};
use crate::tools::parse_args;
use crate::Tool;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};
use std::time::Duration;
use stepper_permission::PermissionRequest;
use stepper_provider::{ToolError, ToolResult, ToolSpec};

const MAX_FETCH: usize = 100_000;
const DEFAULT_TIMEOUT_MS: u64 = 60_000;
const CONNECT_TIMEOUT_MS: u64 = 30_000;
const ALLOW_PRIVATE_ENV: &str = "STEPPER_WEB_FETCH_ALLOW_PRIVATE";

/// Overall request deadline (connect + headers + body). Override with
/// `STEPPER_WEB_FETCH_TIMEOUT_MS`.
fn fetch_timeout() -> Duration {
    let ms = std::env::var("STEPPER_WEB_FETCH_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_TIMEOUT_MS);
    Duration::from_millis(ms)
}

fn is_private_ipv4(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_link_local()
        || ip.is_private()
        || (octets[0] == 100 && (octets[1] & 0b1100_0000) == 64)
}

fn is_private_ipv6(ip: Ipv6Addr) -> bool {
    if let Some(mapped) = ip.to_ipv4_mapped() {
        return is_private_ipv4(mapped);
    }
    let segments = ip.segments();
    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || (segments[0] & 0xffc0) == 0xfe80
        || (segments[0] & 0xfe00) == 0xfc00
}

fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_private_ipv4(v4),
        IpAddr::V6(v6) => is_private_ipv6(v6),
    }
}

/// SSRF pre-flight: resolve the URL's host (literal IPs directly, hostnames via
/// DNS) and refuse private/internal ranges — loopback, link-local (cloud
/// metadata), RFC1918, CGNAT, ULA. `STEPPER_WEB_FETCH_ALLOW_PRIVATE=1` skips
/// the rejection for local dev servers.
async fn reject_private_host(url: &str) -> Result<(), ToolError> {
    if std::env::var(ALLOW_PRIVATE_ENV).is_ok_and(|v| v == "1") {
        return Ok(());
    }
    let parsed = reqwest::Url::parse(url)
        .map_err(|e| ToolError::InvalidArgs(format!("bad url: {e}")))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| ToolError::InvalidArgs("url has no host".into()))?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    let port = parsed.port_or_known_default().unwrap_or(80);

    let addrs = if let Ok(ip) = host.parse::<IpAddr>() {
        vec![ip]
    } else {
        let target = format!("{host}:{port}");
        tokio::task::spawn_blocking(move || {
            target
                .to_socket_addrs()
                .map(|it| it.map(|a| a.ip()).collect::<Vec<_>>())
        })
        .await
        .map_err(|e| ToolError::Execution(e.to_string()))?
        .map_err(|e| ToolError::Execution(format!("dns resolve {host}: {e}")))?
    };
    if addrs.is_empty() {
        return Err(ToolError::Execution(format!("dns resolve {host}: no addresses")));
    }
    if let Some(ip) = addrs.into_iter().find(|ip| is_private_ip(*ip)) {
        return Err(ToolError::Denied(format!(
            "refusing to fetch {url}: {host} resolves to private/internal address {ip} \
             (set {ALLOW_PRIVATE_ENV}=1 to allow local dev servers)"
        )));
    }
    Ok(())
}

pub struct WebFetch {
    spec: ToolSpec,
}

#[derive(Deserialize)]
struct Args {
    url: String,
}

impl Default for WebFetch {
    fn default() -> Self {
        WebFetch {
            spec: ToolSpec {
                name: "web_fetch".into(),
                description: "Fetch a URL over HTTP(S) and return its text body (truncated). \
                              Redirects are not followed: a 30x returns the Location so it can \
                              be fetched explicitly."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": { "url": {"type": "string"} },
                    "required": ["url"]
                }),
                read_only: true,
                parallel_safe: true,
            },
        }
    }
}

#[async_trait]
impl Tool for WebFetch {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn call(&self, args: Value, cx: &ToolCx) -> Result<ToolResult, ToolError> {
        let a: Args = parse_args(args)?;
        if !(a.url.starts_with("http://") || a.url.starts_with("https://")) {
            return Err(ToolError::InvalidArgs("url must be http(s)".into()));
        }
        cx.gate(
            PermissionRequest::WebFetch(a.url.clone()),
            Approval::Command {
                command: format!("web_fetch {}", a.url),
                outside_project: true,
            },
        )
        .await?;

        reject_private_host(&a.url).await?;

        let timeout = fetch_timeout();
        let client = reqwest::Client::builder()
            .user_agent(concat!("stepper/", env!("CARGO_PKG_VERSION")))
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_millis(CONNECT_TIMEOUT_MS).min(timeout))
            .timeout(timeout)
            .build()
            .map_err(|e| ToolError::Execution(e.to_string()))?;

        let response = tokio::select! {
            _ = cx.cancel.cancelled() => return Err(ToolError::Execution("fetch cancelled".into())),
            r = client.get(&a.url).send() => r.map_err(|e| ToolError::Execution(e.to_string()))?,
        };

        let status = response.status();
        if status.is_redirection() {
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("<missing Location header>");
            return Err(ToolError::Execution(format!(
                "fetch {} returned {status} redirect to {location}; redirects are not followed \
                 — fetch that url explicitly",
                a.url
            )));
        }

        let mut body = tokio::select! {
            _ = cx.cancel.cancelled() => return Err(ToolError::Execution("fetch cancelled".into())),
            _ = tokio::time::sleep(timeout) => return Err(ToolError::Execution(format!(
                "fetch body timed out after {}ms", timeout.as_millis()
            ))),
            r = response.text() => r.map_err(|e| ToolError::Execution(e.to_string()))?,
        };
        let truncated = body.len() > MAX_FETCH;
        if truncated {
            crate::truncate_on_char_boundary(&mut body, MAX_FETCH);
        }
        Ok(ToolResult {
            content: vec![stepper_provider::ToolContent::text(format!(
                "[{status}]\n{body}"
            ))],
            is_error: !status.is_success(),
            truncated,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_ranges_are_rejected() {
        for ip in [
            "127.0.0.1",
            "0.0.0.0",
            "255.255.255.255",
            "224.0.0.1",
            "169.254.169.254",
            "10.0.0.1",
            "172.16.0.1",
            "192.168.1.1",
            "100.64.0.1",
            "::1",
            "::",
            "fe80::1",
            "fc00::1",
            "fd12::1",
            "ff02::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
        ] {
            assert!(is_private_ip(ip.parse().unwrap()), "{ip} must be private");
        }
    }

    #[test]
    fn public_addresses_are_allowed() {
        for ip in ["93.184.216.34", "8.8.8.8", "1.1.1.1", "2606:4700:4700::1111"] {
            assert!(!is_private_ip(ip.parse().unwrap()), "{ip} must be public");
        }
    }

    #[test]
    fn cgnat_boundaries() {
        assert!(is_private_ip("100.64.0.0".parse().unwrap()));
        assert!(is_private_ip("100.127.255.255".parse().unwrap()));
        assert!(!is_private_ip("100.63.255.255".parse().unwrap()));
        assert!(!is_private_ip("100.128.0.0".parse().unwrap()));
    }
}
