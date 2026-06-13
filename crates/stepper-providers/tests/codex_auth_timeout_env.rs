//! `STEPPER_CODEX_AUTH_TIMEOUT_MS` belt over token refresh: a token endpoint
//! that accepts the TCP connection and then never answers must fail within the
//! deadline instead of hanging the refresh forever. Kept in its own test
//! binary: the env var is process-global.

use std::net::TcpListener;
use stepper_providers::codex::store::CodexCredentials;
use stepper_providers::{CodexTokenStore, ProviderError};

#[tokio::test(flavor = "multi_thread")]
async fn a_silent_token_endpoint_times_out_via_the_env_tunable_belt() {
    // SAFETY: single test in this binary setting a process env var it then removes.
    unsafe {
        std::env::set_var("STEPPER_CODEX_AUTH_TIMEOUT_MS", "300");
    }

    // Bound but never accepted: the OS completes the TCP handshake, then the
    // client waits forever for the TLS ServerHello — a realistic silent stall.
    let silent = TcpListener::bind("127.0.0.1:0").expect("bind");
    let silent_addr = silent.local_addr().expect("addr");

    let client = reqwest::Client::builder()
        .resolve("auth.openai.com", silent_addr)
        .build()
        .expect("client builds");
    let creds = CodexCredentials {
        access_token: "stale".to_string().into(),
        refresh_token: "refresh".to_string().into(),
        id_token: "id".to_string().into(),
        account_id: "acct".to_string(),
        expires_at: u64::MAX,
        last_refresh: 0,
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let store = CodexTokenStore::create(dir.path().join("codex-auth.json"), client, creds)
        .expect("create store");

    let start = std::time::Instant::now();
    let err = store
        .force_refresh()
        .await
        .expect_err("the stalled refresh must fail");
    let elapsed = start.elapsed();

    unsafe {
        std::env::remove_var("STEPPER_CODEX_AUTH_TIMEOUT_MS");
    }
    drop(silent);

    match err {
        ProviderError::Auth(m) => {
            assert!(m.contains("timed out"), "got: {m}");
            assert!(m.contains("300ms"), "the deadline reflects the env value: {m}");
        }
        other => panic!("expected an Auth timeout, got {other:?}"),
    }
    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "the 300ms belt must fire fast, took {elapsed:?}"
    );
}
