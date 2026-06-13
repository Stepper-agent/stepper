use crate::codex::store::{self, now_unix, CodexCredentials};
use crate::codex::{
    auth_http_timeout, AUTHORIZE_URL, CLIENT_ID, DEFAULT_PORT, FALLBACK_PORT, SCOPE, TOKEN_URL,
};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use secrecy::SecretString;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::Path;
use std::time::{Duration, Instant};
use stepper_provider::ProviderError;
use url::Url;

const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

pub(crate) struct IdTokenClaims {
    pub account_id: String,
    pub exp: u64,
}

/// Run the full Authorization-Code + PKCE login against ChatGPT, persisting the
/// resulting credentials to `store_path`. Returns the resolved
/// `chatgpt_account_id`. Blocks on a localhost callback (ports 1455/1457).
pub async fn login(client: &reqwest::Client, store_path: &Path) -> Result<String, ProviderError> {
    let verifier = random_b64url(32);
    let challenge = pkce_challenge(&verifier);
    let state = random_b64url(24);

    let (server, port) = bind_callback()?;
    let redirect_uri = format!("http://localhost:{port}/auth/callback");
    let authorize_url = build_authorize_url(&redirect_uri, &challenge, &state)?;

    println!("Opening your browser to sign in with ChatGPT…");
    println!("If it doesn't open, visit:\n{authorize_url}\n");
    let _ = open::that(&authorize_url);

    let (code, returned_state) = wait_for_callback(server).await?;
    if returned_state != state {
        return Err(ProviderError::Auth(
            "oauth state mismatch — possible CSRF, aborting".into(),
        ));
    }

    let tokens = exchange_code(client, &code, &redirect_uri, &verifier).await?;
    let claims = decode_id_token(&tokens.id_token)?;

    let creds = CodexCredentials {
        access_token: SecretString::from(tokens.access_token),
        refresh_token: SecretString::from(tokens.refresh_token),
        id_token: SecretString::from(tokens.id_token),
        account_id: claims.account_id.clone(),
        expires_at: claims.exp,
        last_refresh: now_unix(),
    };
    store::persist(store_path, &creds)?;
    Ok(claims.account_id)
}

fn bind_callback() -> Result<(tiny_http::Server, u16), ProviderError> {
    for port in [DEFAULT_PORT, FALLBACK_PORT] {
        if let Ok(server) = tiny_http::Server::http(("127.0.0.1", port)) {
            return Ok((server, port));
        }
    }
    Err(ProviderError::Auth(format!(
        "could not bind oauth callback on 127.0.0.1:{DEFAULT_PORT} or :{FALLBACK_PORT}"
    )))
}

fn build_authorize_url(
    redirect_uri: &str,
    challenge: &str,
    state: &str,
) -> Result<String, ProviderError> {
    let mut u = Url::parse(AUTHORIZE_URL).map_err(|e| ProviderError::Auth(e.to_string()))?;
    u.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", SCOPE)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state)
        .append_pair("id_token_add_organizations", "true")
        .append_pair("originator", "codex_cli_rs");
    Ok(u.to_string())
}

async fn wait_for_callback(
    server: tiny_http::Server,
) -> Result<(String, String), ProviderError> {
    let join = tokio::task::spawn_blocking(move || -> Result<(String, String), String> {
        let deadline = Instant::now() + LOGIN_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err("timed out waiting for the browser callback".into());
            }
            match server.recv_timeout(remaining.min(Duration::from_secs(1))) {
                Ok(Some(req)) => {
                    let url = req.url().to_string();
                    if url.starts_with("/auth/callback") {
                        let result = parse_callback(&url);
                        let html = "<html><body style=\"font-family:sans-serif\">\
                            <h3>stepper</h3><p>Sign-in complete. You can close this tab.</p>\
                            </body></html>";
                        let header = "Content-Type: text/html"
                            .parse::<tiny_http::Header>()
                            .expect("valid header");
                        let _ = req.respond(
                            tiny_http::Response::from_string(html).with_header(header),
                        );
                        return result;
                    }
                    let _ = req.respond(tiny_http::Response::from_string(
                        "stepper auth: waiting for the OpenAI callback…",
                    ));
                }
                Ok(None) => continue,
                Err(e) => return Err(e.to_string()),
            }
        }
    });

    join.await
        .map_err(|e| ProviderError::Auth(format!("callback task failed: {e}")))?
        .map_err(ProviderError::Auth)
}

fn parse_callback(url: &str) -> Result<(String, String), String> {
    let parsed = Url::parse(&format!("http://localhost{url}")).map_err(|e| e.to_string())?;
    let mut code = None;
    let mut state = None;
    for (k, v) in parsed.query_pairs() {
        match k.as_ref() {
            "code" => code = Some(v.into_owned()),
            "state" => state = Some(v.into_owned()),
            "error" => return Err(format!("authorization error: {v}")),
            _ => {}
        }
    }
    match (code, state) {
        (Some(c), Some(s)) => Ok((c, s)),
        _ => Err("callback missing code or state".into()),
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    id_token: String,
    access_token: String,
    refresh_token: String,
}

async fn exchange_code(
    client: &reqwest::Client,
    code: &str,
    redirect_uri: &str,
    verifier: &str,
) -> Result<TokenResponse, ProviderError> {
    let form = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", CLIENT_ID),
        ("code_verifier", verifier),
    ];
    let deadline = auth_http_timeout();
    // Belt over the client's own timeout: the injected client may not carry one.
    let exchange = async {
        let resp = client
            .post(TOKEN_URL)
            .form(&form)
            .send()
            .await
            .map_err(|e| ProviderError::Auth(format!("token exchange request failed: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(ProviderError::Auth(format!(
                "token exchange rejected (status {status}): {}",
                body.chars().take(300).collect::<String>()
            )));
        }
        resp.json::<TokenResponse>()
            .await
            .map_err(|e| ProviderError::Auth(format!("bad token response: {e}")))
    };
    tokio::time::timeout(deadline, exchange)
        .await
        .map_err(|_| {
            ProviderError::Auth(format!(
                "token exchange timed out after {}ms",
                deadline.as_millis()
            ))
        })?
}

pub(crate) fn decode_id_token(token: &str) -> Result<IdTokenClaims, ProviderError> {
    let payload = jwt_payload(token)?;
    let account_id = payload
        .get("https://api.openai.com/auth")
        .and_then(|a| a.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ProviderError::Auth("id_token is missing the chatgpt_account_id claim".into()))?
        .to_string();
    let exp = payload
        .get("exp")
        .and_then(Value::as_u64)
        .ok_or_else(|| ProviderError::Auth("id_token is missing the exp claim".into()))?;
    Ok(IdTokenClaims { account_id, exp })
}

pub(crate) fn jwt_exp(token: &str) -> Result<u64, ProviderError> {
    jwt_payload(token)?
        .get("exp")
        .and_then(Value::as_u64)
        .ok_or_else(|| ProviderError::Auth("no exp claim in token".into()))
}

/// Decode-only JWT payload extraction. The signature is NOT verified — the
/// fixed https `TOKEN_URL` (fetched by a no-redirect client) is the trust
/// anchor — but an `alg: none` header is rejected outright: an unsigned token
/// must never be trusted for `account_id`/`exp`, whatever the channel.
fn jwt_payload(token: &str) -> Result<Value, ProviderError> {
    let mut segments = token.split('.');
    let header_seg = segments
        .next()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ProviderError::Auth("malformed JWT".into()))?;
    let payload_seg = segments
        .next()
        .ok_or_else(|| ProviderError::Auth("malformed JWT".into()))?;
    let header_bytes = URL_SAFE_NO_PAD
        .decode(header_seg)
        .map_err(|e| ProviderError::Auth(format!("JWT header base64 decode: {e}")))?;
    let header: Value = serde_json::from_slice(&header_bytes)
        .map_err(|e| ProviderError::Auth(format!("JWT header json: {e}")))?;
    let alg = header.get("alg").and_then(Value::as_str).unwrap_or("");
    if alg.trim().is_empty() || alg.trim().eq_ignore_ascii_case("none") {
        return Err(ProviderError::Auth(
            "JWT with alg 'none' (unsigned) is rejected".into(),
        ));
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(payload_seg)
        .map_err(|e| ProviderError::Auth(format!("JWT base64 decode: {e}")))?;
    serde_json::from_slice(&bytes).map_err(|e| ProviderError::Auth(format!("JWT payload json: {e}")))
}

fn pkce_challenge(verifier: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(hasher.finalize())
}

fn random_b64url(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).expect("os rng");
    URL_SAFE_NO_PAD.encode(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_id_token() -> String {
        // RS256 header: decode-only path accepts it (signature unverified by
        // design); only `alg: none` is rejected.
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256"}"#);
        let payload = URL_SAFE_NO_PAD.encode(
            br#"{"exp":1893456000,"https://api.openai.com/auth":{"chatgpt_account_id":"acct_xyz"}}"#,
        );
        format!("{header}.{payload}.")
    }

    #[test]
    fn decodes_account_id_and_exp_from_id_token() {
        let claims = decode_id_token(&fake_id_token()).unwrap();
        assert_eq!(claims.account_id, "acct_xyz");
        assert_eq!(claims.exp, 1893456000);
    }

    #[test]
    fn rejects_id_token_missing_required_claims() {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256"}"#);
        // missing chatgpt_account_id
        let no_account = URL_SAFE_NO_PAD.encode(br#"{"exp":1893456000}"#);
        assert!(decode_id_token(&format!("{header}.{no_account}.")).is_err());
        // missing exp
        let no_exp = URL_SAFE_NO_PAD
            .encode(br#"{"https://api.openai.com/auth":{"chatgpt_account_id":"a"}}"#);
        assert!(decode_id_token(&format!("{header}.{no_exp}.")).is_err());
    }

    #[test]
    fn rejects_unsigned_id_token_with_alg_none_case_insensitively() {
        let payload = URL_SAFE_NO_PAD.encode(
            br#"{"exp":1893456000,"https://api.openai.com/auth":{"chatgpt_account_id":"acct_xyz"}}"#,
        );
        for alg_header in [
            br#"{"alg":"none"}"#.as_slice(),
            br#"{"alg":"NONE"}"#.as_slice(),
            br#"{"alg":"None"}"#.as_slice(),
            br#"{"alg":""}"#.as_slice(),
            br#"{}"#.as_slice(),
        ] {
            let header = URL_SAFE_NO_PAD.encode(alg_header);
            let token = format!("{header}.{payload}.");
            let err = decode_id_token(&token).err().expect("unsigned token must be rejected");
            assert!(
                matches!(err, ProviderError::Auth(_)),
                "alg-none rejection surfaces as Auth, got {err:?}"
            );
        }
    }

    #[test]
    fn jwt_exp_also_rejects_alg_none_tokens() {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        let payload = URL_SAFE_NO_PAD.encode(br#"{"exp":1893456000}"#);
        assert!(jwt_exp(&format!("{header}.{payload}.")).is_err());
    }

    #[test]
    fn pkce_challenge_is_stable_and_url_safe() {
        let c = pkce_challenge("verifier123");
        assert!(!c.contains('+') && !c.contains('/') && !c.contains('='));
        assert_eq!(c, pkce_challenge("verifier123"));
    }

    #[test]
    fn authorize_url_carries_required_params() {
        let url = build_authorize_url("http://localhost:1455/auth/callback", "chal", "st").unwrap();
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("client_id=app_EMoamEEZ73f0CkXaXp7hrann"));
        assert!(url.contains("originator=codex_cli_rs"));
        assert!(url.contains("id_token_add_organizations=true"));
    }
}
