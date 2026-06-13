//! Codex credential hardening: `persist()` writes the file 0600 from the first
//! byte (temp file + atomic rename, parent dirs 0700, no readable window), and
//! the factory's auth/token client never follows redirects — the fixed https
//! TOKEN_URL is the trust anchor for the decode-only id_token.

use stepper_providers::codex::store::CodexCredentials;
use stepper_providers::{CodexTokenStore, ProviderFactory};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn creds(access: &str) -> CodexCredentials {
    CodexCredentials {
        access_token: access.to_string().into(),
        refresh_token: "refresh".to_string().into(),
        id_token: "id".to_string().into(),
        account_id: "acct".to_string(),
        expires_at: u64::MAX,
        last_refresh: 0,
    }
}

#[cfg(unix)]
#[test]
fn persist_creates_the_file_0600_and_parent_dirs_0700_with_no_temp_leftovers() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("tempdir");
    let nested_parent = dir.path().join("home").join(".stepper");
    let path = nested_parent.join("codex-auth.json");

    let client = reqwest::Client::new();
    CodexTokenStore::create(&path, client, creds("token-bytes")).expect("create persists");

    let file_mode = std::fs::metadata(&path).expect("file exists").permissions().mode();
    assert_eq!(
        file_mode & 0o777,
        0o600,
        "credential file is owner-only from creation"
    );

    let dir_mode = std::fs::metadata(&nested_parent)
        .expect("parent created")
        .permissions()
        .mode();
    assert_eq!(dir_mode & 0o777, 0o700, "created parent dir is 0700");
    let home_mode = std::fs::metadata(dir.path().join("home"))
        .expect("grandparent created")
        .permissions()
        .mode();
    assert_eq!(home_mode & 0o777, 0o700, "every created ancestor is 0700");

    let raw = std::fs::read_to_string(&path).expect("readable by owner");
    let parsed: serde_json::Value = serde_json::from_str(&raw).expect("valid json");
    assert_eq!(parsed["access_token"], "token-bytes");

    let leftovers: Vec<_> = std::fs::read_dir(&nested_parent)
        .expect("listable")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .filter(|name| name != "codex-auth.json")
        .collect();
    assert!(
        leftovers.is_empty(),
        "the temp file was renamed away, found {leftovers:?}"
    );
}

#[cfg(unix)]
#[test]
fn persist_overwrites_an_existing_file_atomically_and_keeps_0600() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("codex-auth.json");
    let client = reqwest::Client::new();

    CodexTokenStore::create(&path, client.clone(), creds("first")).expect("first persist");
    CodexTokenStore::create(&path, client, creds("second")).expect("second persist");

    let raw = std::fs::read_to_string(&path).expect("readable");
    let parsed: serde_json::Value = serde_json::from_str(&raw).expect("valid json");
    assert_eq!(parsed["access_token"], "second", "rename replaced the old file");

    let mode = std::fs::metadata(&path).expect("exists").permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[tokio::test]
async fn the_factory_auth_client_never_follows_redirects() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(
            ResponseTemplate::new(302).insert_header("location", "https://evil.example/steal"),
        )
        .mount(&server)
        .await;

    let factory = ProviderFactory::new().expect("factory");
    let resp = factory
        .client()
        .post(format!("{}/oauth/token", server.uri()))
        .form(&[("grant_type", "authorization_code")])
        .send()
        .await
        .expect("the redirect response itself is returned");
    assert_eq!(
        resp.status().as_u16(),
        302,
        "Policy::none surfaces the 302 instead of following it"
    );
}
