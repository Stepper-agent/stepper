//! Hermetic `McpManager::connect` behaviour for an unreachable stdio server: a
//! configured server whose `command` is a guaranteed-missing binary must be
//! logged and skipped (manager.rs `Err` branch) without panicking or taking the
//! manager down. NO network, NO real child process is ever spawned successfully.

use std::collections::BTreeMap;
use stepper_config::McpServerConfig;
use stepper_mcp::McpManager;

fn missing_command_server() -> McpServerConfig {
    McpServerConfig {
        command: Some(
            "stepper-mcp-test-definitely-not-a-real-binary-7f3a9c2e".to_string(),
        ),
        ..McpServerConfig::default()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn bogus_stdio_command_is_skipped_and_connect_still_succeeds() {
    let mut servers = BTreeMap::new();
    servers.insert("ghost".to_string(), missing_command_server());

    let manager = McpManager::connect(&servers).await;

    assert!(
        manager.is_empty(),
        "an unspawnable server must contribute no tools"
    );
    assert!(manager.tools().is_empty());
    assert!(manager.tool_names().is_empty());
    assert_eq!(
        manager.server_of("mcp__ghost__anything"),
        None,
        "no origin should be recorded for the skipped server"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn other_servers_survive_a_single_bogus_server() {
    let mut servers = BTreeMap::new();
    servers.insert("ghost-a".to_string(), missing_command_server());
    servers.insert("ghost-b".to_string(), missing_command_server());

    let manager = McpManager::connect(&servers).await;

    assert!(
        manager.is_empty(),
        "every unspawnable server is skipped independently"
    );
    assert_eq!(manager.server_of("mcp__ghost-a__x"), None);
    assert_eq!(manager.server_of("mcp__ghost-b__x"), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn empty_server_map_connects_to_an_empty_manager() {
    let manager = McpManager::connect(&BTreeMap::new()).await;
    assert!(manager.is_empty());
    assert!(manager.tools().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_that_never_completes_the_handshake_times_out_and_is_skipped() {
    // `sleep` spawns fine but never speaks MCP; the connect deadline must skip it
    // quickly rather than hang the whole manager/CLI at startup.
    // SAFETY: single-threaded test body setting a process env var it then removes.
    unsafe {
        std::env::set_var("STEPPER_MCP_CONNECT_TIMEOUT_MS", "300");
    }
    let mut servers = BTreeMap::new();
    servers.insert(
        "hung".to_string(),
        McpServerConfig {
            command: Some("sleep".to_string()),
            args: vec!["30".to_string()],
            ..McpServerConfig::default()
        },
    );

    let start = std::time::Instant::now();
    let manager = McpManager::connect(&servers).await;
    let elapsed = start.elapsed();
    unsafe {
        std::env::remove_var("STEPPER_MCP_CONNECT_TIMEOUT_MS");
    }

    assert!(manager.is_empty(), "a hung server must contribute no tools");
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "connect must time out, not hang: took {elapsed:?}"
    );
}
