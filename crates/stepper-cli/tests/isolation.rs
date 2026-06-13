//! Workspace isolation invariants asserted from `cargo metadata` (ARCHITECTURE
//! §1): `stepper-tui` depends only on `stepper-protocol` (+ the ratatui UI stack)
//! and never transitively pulls in the core/provider/http layers, and
//! `stepper-protocol` stays free of http (reqwest), the async runtime, and clap.
//! A leak here means a layering or dependency-boundary regression.

use serde_json::Value;
use std::collections::{HashMap, HashSet};

fn metadata() -> Value {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let output = std::process::Command::new(cargo)
        .args(["metadata", "--format-version", "1"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run cargo metadata");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("parse cargo metadata json")
}

fn id_to_name(meta: &Value) -> HashMap<String, String> {
    meta["packages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            (
                p["id"].as_str().unwrap().to_string(),
                p["name"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

fn node_id_for(meta: &Value, name: &str) -> String {
    meta["resolve"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["id"].as_str().unwrap())
        .find(|id| id_to_name(meta).get(*id).map(String::as_str) == Some(name))
        .unwrap_or_else(|| panic!("no resolve node for {name}"))
        .to_string()
}

/// Names of every package reachable from `root` over **normal** (non-dev,
/// non-build) dependency edges — the closure that actually ships in the build.
fn runtime_closure(meta: &Value, root_name: &str) -> HashSet<String> {
    let names = id_to_name(meta);
    let edges: HashMap<&str, Vec<&str>> = meta["resolve"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| {
            let id = n["id"].as_str().unwrap();
            let deps = n["deps"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|d| {
                    d["dep_kinds"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|k| k["kind"].is_null())
                })
                .map(|d| d["pkg"].as_str().unwrap())
                .collect();
            (id, deps)
        })
        .collect();

    let mut seen = HashSet::new();
    let mut stack = vec![node_id_for(meta, root_name)];
    while let Some(id) = stack.pop() {
        for dep in edges.get(id.as_str()).into_iter().flatten() {
            if seen.insert(names[*dep].clone()) {
                stack.push(dep.to_string());
            }
        }
    }
    seen
}

/// Normal (non-dev, non-build) manifest dependency names of one workspace package.
fn manifest_normal_deps(meta: &Value, pkg: &str) -> Vec<(String, Vec<String>)> {
    meta["packages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"].as_str() == Some(pkg))
        .unwrap_or_else(|| panic!("no package {pkg}"))["dependencies"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|d| d["kind"].is_null())
        .map(|d| {
            let feats = d["features"]
                .as_array()
                .map(|a| a.iter().map(|f| f.as_str().unwrap().to_string()).collect())
                .unwrap_or_default();
            (d["name"].as_str().unwrap().to_string(), feats)
        })
        .collect()
}

#[test]
fn tui_only_depends_on_protocol_among_workspace_crates() {
    let meta = metadata();
    let deps = manifest_normal_deps(&meta, "stepper-tui");
    let stepper_deps: Vec<&String> = deps
        .iter()
        .map(|(n, _)| n)
        .filter(|n| n.starts_with("stepper-"))
        .collect();
    assert_eq!(
        stepper_deps,
        vec!["stepper-protocol"],
        "stepper-tui must depend on no workspace crate but stepper-protocol"
    );
    for forbidden in ["reqwest", "clap", "rmcp", "eventsource-stream"] {
        assert!(
            !deps.iter().any(|(n, _)| n == forbidden),
            "stepper-tui must not declare a direct {forbidden} dependency"
        );
    }
}

#[test]
fn tui_runtime_closure_excludes_http_and_upper_layers() {
    let meta = metadata();
    let closure = runtime_closure(&meta, "stepper-tui");
    for forbidden in [
        "reqwest",
        "eventsource-stream",
        "rmcp",
        "stepper-core",
        "stepper-config",
        "stepper-provider",
        "stepper-providers",
        "stepper-tools",
        "stepper-mcp",
        "stepper-permission",
        "stepper-cli",
    ] {
        assert!(
            !closure.contains(forbidden),
            "stepper-tui runtime closure must not contain {forbidden}; closure = {closure:?}"
        );
    }
    assert!(
        closure.contains("stepper-protocol") && closure.contains("ratatui"),
        "sanity: tui closure should still contain protocol + ratatui: {closure:?}"
    );
}

#[test]
fn protocol_is_http_runtime_and_clap_free() {
    let meta = metadata();

    let deps = manifest_normal_deps(&meta, "stepper-protocol");
    let tokio = deps
        .iter()
        .find(|(n, _)| n == "tokio")
        .expect("protocol depends on tokio");
    for runtime_feature in ["rt", "rt-multi-thread", "macros", "net", "process", "io-util"] {
        assert!(
            !tokio.1.iter().any(|f| f == runtime_feature),
            "stepper-protocol's tokio must stay runtime-free (no {runtime_feature}); got {:?}",
            tokio.1
        );
    }

    let closure = runtime_closure(&meta, "stepper-protocol");
    for forbidden in [
        "reqwest",
        "clap",
        "rmcp",
        "eventsource-stream",
        "hyper",
        "stepper-tui",
        "stepper-core",
        "stepper-providers",
        "stepper-tools",
        "stepper-mcp",
        "stepper-config",
        "stepper-permission",
    ] {
        assert!(
            !closure.contains(forbidden),
            "stepper-protocol runtime closure must not contain {forbidden}; closure = {closure:?}"
        );
    }
}
