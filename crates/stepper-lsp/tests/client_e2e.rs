//! End-to-end client/manager test against a hermetic fake language server
//! (`CARGO_BIN_EXE_fake_lsp_server`) that pushes a diagnostic on open.

use stepper_lsp::catalog::ServerSpec;
use stepper_lsp::LspManager;

fn fake_spec(extensions: Vec<String>) -> ServerSpec {
    ServerSpec {
        id: "fake".into(),
        command: vec![env!("CARGO_BIN_EXE_fake_lsp_server").into()],
        extensions,
        env: Vec::new(),
        initialization: None,
    }
}

#[tokio::test]
async fn manager_gathers_pushed_diagnostics_after_an_edit() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("x.demo");
    std::fs::write(&file, "code\n").unwrap();

    let mgr = LspManager::new(dir.path().to_path_buf(), vec![fake_spec(vec![".demo".into()])]);
    let report = mgr.diagnostics_after_edit(&file).await;

    assert!(report.contains("fake error"), "diagnostics surfaced: {report}");
    assert!(report.contains("<diagnostics file=\"x.demo\">"), "got: {report}");
    assert!(report.contains("ERROR [1:1]"), "1-based location: {report}");
    mgr.shutdown().await;
}

#[tokio::test]
async fn a_re_edit_reuses_the_server_and_still_reports() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("y.demo");
    std::fs::write(&file, "first\n").unwrap();
    let mgr = LspManager::new(dir.path().to_path_buf(), vec![fake_spec(vec![".demo".into()])]);

    assert!(mgr.diagnostics_after_edit(&file).await.contains("fake error"));
    // Second edit → didChange path; the server pushes again.
    std::fs::write(&file, "second\n").unwrap();
    assert!(mgr.diagnostics_after_edit(&file).await.contains("fake error"));
    mgr.shutdown().await;
}

#[tokio::test]
async fn a_non_matching_extension_yields_no_report() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("z.other");
    std::fs::write(&file, "code\n").unwrap();
    let mgr = LspManager::new(dir.path().to_path_buf(), vec![fake_spec(vec![".demo".into()])]);
    assert_eq!(mgr.diagnostics_after_edit(&file).await, "");
    mgr.shutdown().await;
}

#[tokio::test]
async fn an_empty_manager_is_disabled() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a.demo");
    std::fs::write(&file, "code\n").unwrap();
    let mgr = LspManager::new(dir.path().to_path_buf(), Vec::new());
    assert!(mgr.is_empty());
    assert_eq!(mgr.diagnostics_after_edit(&file).await, "");
}
