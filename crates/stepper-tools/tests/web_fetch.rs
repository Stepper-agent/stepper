//! `web_fetch` behavior against a hermetic wiremock server: the permission gate
//! decides whether the fetch happens, the SSRF pre-flight rejects private
//! addresses (wiremock binds loopback, so mock-backed tests opt in with
//! `STEPPER_WEB_FETCH_ALLOW_PRIVATE=1`), and redirects surface instead of being
//! followed. A process-wide lock serializes every test that touches the env vars.

use async_trait::async_trait;
use serde_json::json;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;
use stepper_permission::{Decision, PermissionMode, RuleSet};
use stepper_tools::{Approval, Approver, ToolCx, ToolError, ToolRegistry};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const ALLOW_PRIVATE: &str = "STEPPER_WEB_FETCH_ALLOW_PRIVATE";
const TIMEOUT_MS: &str = "STEPPER_WEB_FETCH_TIMEOUT_MS";

static ENV_LOCK: Mutex<()> = Mutex::new(());

struct EnvVars {
    _lock: MutexGuard<'static, ()>,
}

impl EnvVars {
    fn set(vars: &[(&'static str, &str)]) -> Self {
        let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: ENV_LOCK serializes every test that mutates these
        // process-global vars; they are reset here and on drop.
        unsafe {
            std::env::remove_var(ALLOW_PRIVATE);
            std::env::remove_var(TIMEOUT_MS);
            for (key, value) in vars {
                std::env::set_var(key, value);
            }
        }
        EnvVars { _lock: lock }
    }
}

impl Drop for EnvVars {
    fn drop(&mut self) {
        // SAFETY: still holding ENV_LOCK via `_lock`.
        unsafe {
            std::env::remove_var(ALLOW_PRIVATE);
            std::env::remove_var(TIMEOUT_MS);
        }
    }
}

struct Recording {
    decision: Decision,
    calls: Mutex<usize>,
}

impl Recording {
    fn new(decision: Decision) -> Arc<Self> {
        Arc::new(Recording {
            decision,
            calls: Mutex::new(0),
        })
    }

    fn calls(&self) -> usize {
        *self.calls.lock().unwrap()
    }
}

#[async_trait]
impl Approver for Recording {
    async fn request(&self, _approval: Approval) -> Decision {
        *self.calls.lock().unwrap() += 1;
        self.decision
    }
}

fn cx_with(mode: PermissionMode, rules: RuleSet, approver: Arc<dyn Approver>) -> ToolCx {
    let dir = std::env::temp_dir();
    ToolCx {
        cwd: dir.clone(),
        project_root: dir,
        home: None,
        mode,
        live_mode: None,
        rules: Arc::new(rules),
        approver,
        cancel: CancellationToken::new(),
        sandbox_writable_roots: None,
    }
}

fn allow_all_fetch_cx(approver: Arc<dyn Approver>) -> ToolCx {
    cx_with(
        PermissionMode::Auto,
        RuleSet::from_lists(&["WebFetch(*)".into()], &[], &[]),
        approver,
    )
}

#[tokio::test]
async fn web_fetch_returns_body_when_allow_rule_matches() {
    let _env = EnvVars::set(&[(ALLOW_PRIVATE, "1")]);
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/page"))
        .respond_with(ResponseTemplate::new(200).set_body_string("MOCK BODY CONTENT"))
        .mount(&server)
        .await;

    let reg = ToolRegistry::builtins();
    let approver = Recording::new(Decision::Deny);
    let cx = allow_all_fetch_cx(approver.clone());

    let url = format!("{}/page", server.uri());
    let result = reg
        .get("web_fetch")
        .unwrap()
        .call(json!({"url": url}), &cx)
        .await
        .unwrap();

    let text = result.content_text();
    assert!(text.contains("MOCK BODY CONTENT"), "got {text}");
    assert!(text.contains("[200"));
    assert!(!result.is_error);
    assert_eq!(approver.calls(), 0);
}

#[tokio::test]
async fn web_fetch_is_gated_and_not_fetched_when_approver_denies() {
    let _env = EnvVars::set(&[(ALLOW_PRIVATE, "1")]);
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/secret"))
        .respond_with(ResponseTemplate::new(200).set_body_string("LEAK"))
        .expect(0)
        .mount(&server)
        .await;

    let reg = ToolRegistry::builtins();
    let approver = Recording::new(Decision::Deny);
    // Gated mode: Auto auto-allows WebFetch now, so a gated mode is needed to
    // route it through the approver (which denies → not fetched).
    let cx = cx_with(PermissionMode::Default, RuleSet::default(), approver.clone());

    let url = format!("{}/secret", server.uri());
    let err = reg
        .get("web_fetch")
        .unwrap()
        .call(json!({"url": url}), &cx)
        .await
        .unwrap_err();

    assert!(matches!(err, ToolError::Denied(_)));
    assert_eq!(approver.calls(), 1);
}

#[tokio::test]
async fn web_fetch_marks_error_on_non_success_status() {
    let _env = EnvVars::set(&[(ALLOW_PRIVATE, "1")]);
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/missing"))
        .respond_with(ResponseTemplate::new(404).set_body_string("not here"))
        .mount(&server)
        .await;

    let reg = ToolRegistry::builtins();
    let approver = Recording::new(Decision::Allow);
    // Gated mode so the fetch is routed through the approver (calls()==1).
    let cx = cx_with(PermissionMode::Default, RuleSet::default(), approver.clone());

    let url = format!("{}/missing", server.uri());
    let result = reg
        .get("web_fetch")
        .unwrap()
        .call(json!({"url": url}), &cx)
        .await
        .unwrap();

    assert!(result.is_error);
    assert!(result.content_text().contains("[404"));
    assert_eq!(approver.calls(), 1);
}

#[tokio::test]
async fn web_fetch_rejects_private_and_metadata_addresses() {
    let _env = EnvVars::set(&[]);
    let reg = ToolRegistry::builtins();
    let approver = Recording::new(Decision::Deny);
    let cx = allow_all_fetch_cx(approver.clone());

    for url in [
        "http://169.254.169.254/latest/meta-data/",
        "http://127.0.0.1:9/x",
        "http://10.0.0.1/",
        "http://192.168.1.1/",
        "http://[::1]/",
        "http://[fc00::1]/",
    ] {
        let err = reg
            .get("web_fetch")
            .unwrap()
            .call(json!({"url": url}), &cx)
            .await
            .unwrap_err();
        match err {
            ToolError::Denied(msg) => {
                assert!(msg.contains("private/internal"), "{url}: got {msg}");
                assert!(msg.contains(ALLOW_PRIVATE), "{url}: got {msg}");
            }
            other => panic!("{url}: expected SSRF rejection, got {other:?}"),
        }
    }
    assert_eq!(approver.calls(), 0);
}

#[tokio::test]
async fn web_fetch_rejects_loopback_mock_without_allow_private() {
    let _env = EnvVars::set(&[]);
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/page"))
        .respond_with(ResponseTemplate::new(200).set_body_string("MUST NOT LEAK"))
        .expect(0)
        .mount(&server)
        .await;

    let reg = ToolRegistry::builtins();
    let approver = Recording::new(Decision::Deny);
    let cx = allow_all_fetch_cx(approver.clone());

    let url = format!("{}/page", server.uri());
    let err = reg
        .get("web_fetch")
        .unwrap()
        .call(json!({"url": url}), &cx)
        .await
        .unwrap_err();

    match err {
        ToolError::Denied(msg) => assert!(msg.contains("private/internal"), "got {msg}"),
        other => panic!("expected SSRF rejection, got {other:?}"),
    }
}

#[tokio::test]
async fn web_fetch_public_address_passes_ssrf_preflight_and_times_out() {
    // TEST-NET-1 (192.0.2.1) is public address space that is guaranteed
    // unroutable, so the pre-flight must let it through and the attempt then
    // fails on the (env-tuned) timeout without any network answer.
    let _env = EnvVars::set(&[(TIMEOUT_MS, "300")]);
    let reg = ToolRegistry::builtins();
    let approver = Recording::new(Decision::Deny);
    let cx = allow_all_fetch_cx(approver.clone());

    let start = Instant::now();
    let err = reg
        .get("web_fetch")
        .unwrap()
        .call(json!({"url": "http://192.0.2.1:9/"}), &cx)
        .await
        .unwrap_err();
    let elapsed = start.elapsed();

    match err {
        ToolError::Execution(msg) => assert!(!msg.contains("private/internal"), "got {msg}"),
        ToolError::Denied(msg) => panic!("public address must pass the SSRF pre-flight: {msg}"),
        other => panic!("expected a connect failure, got {other:?}"),
    }
    assert!(elapsed.as_secs() < 10, "timeout env must bound the attempt: {elapsed:?}");
}

#[tokio::test]
async fn web_fetch_surfaces_redirect_location_instead_of_following() {
    let _env = EnvVars::set(&[(ALLOW_PRIVATE, "1")]);
    let server = MockServer::start().await;
    let location = format!("{}/next", server.uri());
    Mock::given(method("GET"))
        .and(path("/redir"))
        .respond_with(ResponseTemplate::new(302).insert_header("Location", location.as_str()))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/next"))
        .respond_with(ResponseTemplate::new(200).set_body_string("MUST NOT FOLLOW"))
        .expect(0)
        .mount(&server)
        .await;

    let reg = ToolRegistry::builtins();
    let approver = Recording::new(Decision::Allow);
    let cx = cx_with(PermissionMode::Auto, RuleSet::default(), approver.clone());

    let url = format!("{}/redir", server.uri());
    let err = reg
        .get("web_fetch")
        .unwrap()
        .call(json!({"url": url}), &cx)
        .await
        .unwrap_err();

    match err {
        ToolError::Execution(msg) => {
            assert!(msg.contains("302"), "got {msg}");
            assert!(msg.contains(&location), "redirect target must surface: {msg}");
        }
        other => panic!("expected redirect error, got {other:?}"),
    }
}
