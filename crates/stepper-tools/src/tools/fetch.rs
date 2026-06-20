use crate::context::{Approval, ToolCx};
use crate::tools::parse_args;
use crate::Tool;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::time::Duration;
use stepper_config::ProxyConfig;
use stepper_permission::PermissionRequest;
use stepper_provider::{ToolError, ToolResult, ToolSpec};

const MAX_FETCH: usize = 100_000;
const DEFAULT_TIMEOUT_MS: u64 = 60_000;
const CONNECT_TIMEOUT_MS: u64 = 30_000;
const ALLOW_PRIVATE_ENV: &str = "STEPPER_WEB_FETCH_ALLOW_PRIVATE";

/// Merge a private CA bundle (`STEPPER_EXTRA_CA_CERTS`, falling back to
/// `NODE_EXTRA_CA_CERTS`) into the platform trust store so web_fetch validates
/// corporate-proxy / self-signed TLS. Additive (system roots still apply) and
/// fail-open: unset/unreadable/unparsable leaves the builder untouched. Proxy
/// needs no code: reqwest honors `HTTP(S)_PROXY`/`NO_PROXY` by default.
fn apply_extra_ca(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    let Some(path) =
        std::env::var_os("STEPPER_EXTRA_CA_CERTS").or_else(|| std::env::var_os("NODE_EXTRA_CA_CERTS"))
    else {
        return builder;
    };
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!("extra CA certificates: cannot read {path:?}: {err}");
            return builder;
        }
    };
    let certs = match reqwest::Certificate::from_pem_bundle(&bytes) {
        Ok(certs) => certs,
        Err(err) => {
            tracing::warn!("extra CA certificates: cannot parse {path:?}: {err}");
            return builder;
        }
    };
    // rustls-platform-verifier can merge EXTRA roots only on these targets; on any
    // other target a non-empty root set makes `.build()` error, so stay on system
    // trust there — keeping the fail-open total (a valid cert never aborts startup).
    #[cfg(any(all(unix, not(target_os = "android")), target_os = "windows"))]
    {
        builder.tls_certs_merge(certs)
    }
    #[cfg(not(any(all(unix, not(target_os = "android")), target_os = "windows")))]
    {
        let _ = certs;
        builder
    }
}

/// Apply an explicit proxy from config. `None`/inactive leaves the builder
/// untouched so reqwest's `HTTP(S)_PROXY`/`NO_PROXY` env default applies; an
/// explicit proxy replaces the env proxy; `disabled: true` forces a direct
/// connection. Fail-open: a malformed proxy URL is logged and skipped.
fn apply_proxy(mut builder: reqwest::ClientBuilder, proxy: Option<&ProxyConfig>) -> reqwest::ClientBuilder {
    let Some(proxy) = proxy.filter(|p| p.is_active()) else {
        return builder;
    };
    if proxy.disabled {
        return builder.no_proxy();
    }
    let no_proxy = || proxy.no_proxy.as_deref().and_then(reqwest::NoProxy::from_string);
    // Scheme-specific proxies before the catch-all `all` — reqwest uses the first
    // matching one, so `all` must come last or it shadows `http`/`https`.
    if let Some(url) = proxy.http.as_deref() {
        match reqwest::Proxy::http(url) {
            Ok(p) => builder = builder.proxy(p.no_proxy(no_proxy())),
            Err(e) => tracing::warn!("proxy: ignoring invalid `http` proxy {url:?}: {e}"),
        }
    }
    if let Some(url) = proxy.https.as_deref() {
        match reqwest::Proxy::https(url) {
            Ok(p) => builder = builder.proxy(p.no_proxy(no_proxy())),
            Err(e) => tracing::warn!("proxy: ignoring invalid `https` proxy {url:?}: {e}"),
        }
    }
    if let Some(url) = proxy.all.as_deref() {
        match reqwest::Proxy::all(url) {
            Ok(p) => builder = builder.proxy(p.no_proxy(no_proxy())),
            Err(e) => tracing::warn!("proxy: ignoring invalid `all` proxy {url:?}: {e}"),
        }
    }
    builder
}

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
///
/// Returns the validated `(host, addr)` to pin the actual request to, so a
/// DNS-rebinding server can't pass this pre-flight with a public IP then have
/// the real connection re-resolve to a private one (TOCTOU). `Ok(None)` means
/// the escape hatch is set — connect unpinned.
async fn reject_private_host(url: &str) -> Result<Option<(String, SocketAddr)>, ToolError> {
    if std::env::var(ALLOW_PRIVATE_ENV).is_ok_and(|v| v == "1") {
        return Ok(None);
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
    if let Some(ip) = addrs.iter().copied().find(|ip| is_private_ip(*ip)) {
        return Err(ToolError::Denied(format!(
            "refusing to fetch {url}: {host} resolves to private/internal address {ip} \
             (set {ALLOW_PRIVATE_ENV}=1 to allow local dev servers)"
        )));
    }
    // No address was private; pin to the first validated one.
    Ok(Some((host, SocketAddr::new(addrs[0], port))))
}

pub struct WebFetch {
    spec: ToolSpec,
    proxy: Option<ProxyConfig>,
}

#[derive(Deserialize)]
struct Args {
    url: String,
}

impl WebFetch {
    /// `web_fetch` routing its request through an explicit `proxy` from config
    /// (`None` keeps reqwest's `HTTP(S)_PROXY`/`NO_PROXY` env default).
    pub fn with_proxy(proxy: Option<ProxyConfig>) -> Self {
        WebFetch { proxy, ..WebFetch::default() }
    }
}

impl Default for WebFetch {
    fn default() -> Self {
        WebFetch {
            proxy: None,
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

        let pin = reject_private_host(&a.url).await?;

        let timeout = fetch_timeout();
        let mut builder = apply_proxy(apply_extra_ca(reqwest::Client::builder()), self.proxy.as_ref())
            .user_agent(concat!("stepper/", env!("CARGO_PKG_VERSION")))
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_millis(CONNECT_TIMEOUT_MS).min(timeout))
            .timeout(timeout);
        // Connect to the exact address validated in the pre-flight (defeats a
        // second, unchecked DNS lookup at connect time).
        if let Some((host, addr)) = pin {
            builder = builder.resolve(&host, addr);
        }
        let client = builder
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

    #[test]
    fn extra_ca_falls_open_and_still_builds() {
        use std::sync::Mutex;
        static ENV_LOCK: Mutex<()> = Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap();
        let prev = std::env::var_os("STEPPER_EXTRA_CA_CERTS");
        unsafe { std::env::set_var("STEPPER_EXTRA_CA_CERTS", "/no/such/ca.pem") };
        assert!(
            apply_extra_ca(reqwest::Client::builder()).build().is_ok(),
            "a missing extra-CA path must fail open, not break web_fetch"
        );
        unsafe {
            match prev {
                Some(v) => std::env::set_var("STEPPER_EXTRA_CA_CERTS", v),
                None => std::env::remove_var("STEPPER_EXTRA_CA_CERTS"),
            }
        }
    }

    #[test]
    fn apply_proxy_handles_none_disabled_explicit_and_garbage() {
        let ok = |b: reqwest::ClientBuilder| b.build().is_ok();
        // None / inactive → env-proxy default preserved (no-op).
        assert!(ok(apply_proxy(reqwest::Client::builder(), None)));
        assert!(ok(apply_proxy(reqwest::Client::builder(), Some(&ProxyConfig::default()))));
        // disabled → direct connection.
        let disabled = ProxyConfig { disabled: true, ..Default::default() };
        assert!(ok(apply_proxy(reqwest::Client::builder(), Some(&disabled))));
        // explicit https proxy with a bypass list.
        let explicit = ProxyConfig {
            https: Some("http://127.0.0.1:8080".into()),
            no_proxy: Some("localhost".into()),
            ..Default::default()
        };
        assert!(ok(apply_proxy(reqwest::Client::builder(), Some(&explicit))));
        // garbage proxy URL → fail open (still builds).
        let garbage = ProxyConfig { all: Some("not a url".into()), ..Default::default() };
        assert!(ok(apply_proxy(reqwest::Client::builder(), Some(&garbage))));
    }

    #[tokio::test]
    async fn preflight_returns_validated_pin_for_public_literal_ip() {
        // A public literal IP needs no DNS; the pre-flight returns it as the pin
        // the client binds to, defeating any second lookup.
        let pin = reject_private_host("http://93.184.216.34:8080/x")
            .await
            .unwrap();
        let (host, addr) = pin.expect("public host yields a pin");
        assert_eq!(host, "93.184.216.34");
        assert_eq!(addr, "93.184.216.34:8080".parse().unwrap());
    }

    #[tokio::test]
    async fn preflight_denies_private_literal_ip() {
        let err = reject_private_host("http://169.254.169.254/latest/meta-data/")
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Denied(_)), "got {err:?}");
    }
}
