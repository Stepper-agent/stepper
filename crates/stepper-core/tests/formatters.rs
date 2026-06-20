//! `resolve_formatters` maps `settings.formatter` (true / false / overrides map)
//! to the active format-on-edit set, mirroring opencode's resolution.

use std::collections::BTreeMap;
use stepper_config::{FormatterConfig, FormatterEntry};
use stepper_core::resolve_formatters;
use stepper_tools::Detect;

#[test]
fn omitted_or_false_disables_all() {
    assert!(resolve_formatters(None).is_empty());
    assert!(resolve_formatters(Some(&FormatterConfig::All(false))).is_empty());
}

#[test]
fn true_enables_every_builtin() {
    let fmts = resolve_formatters(Some(&FormatterConfig::All(true)));
    for name in ["rustfmt", "gofmt", "prettier", "ruff"] {
        assert!(
            fmts.iter().any(|f| f.name == name),
            "built-in {name} is enabled"
        );
    }
}

#[test]
fn map_keeps_builtins_and_honors_disabled() {
    let mut map = BTreeMap::new();
    map.insert(
        "prettier".to_string(),
        FormatterEntry {
            disabled: true,
            ..Default::default()
        },
    );
    let fmts = resolve_formatters(Some(&FormatterConfig::Map(map)));
    assert!(
        fmts.iter().any(|f| f.name == "rustfmt"),
        "other built-ins stay on"
    );
    assert!(
        !fmts.iter().any(|f| f.name == "prettier"),
        "the disabled one is removed"
    );
}

#[test]
fn map_adds_a_custom_formatter() {
    let mut map = BTreeMap::new();
    map.insert(
        "deno-md".to_string(),
        FormatterEntry {
            command: Some(vec!["deno".into(), "fmt".into(), "$FILE".into()]),
            extensions: Some(vec![".md".into()]),
            ..Default::default()
        },
    );
    let fmts = resolve_formatters(Some(&FormatterConfig::Map(map)));
    let custom = fmts
        .iter()
        .find(|f| f.name == "deno-md")
        .expect("custom formatter present");
    assert_eq!(custom.extensions, vec![".md".to_string()]);
    assert!(matches!(&custom.detect, Detect::Command { .. }));
}

#[test]
fn map_overrides_a_builtin_command_and_extensions() {
    let mut map = BTreeMap::new();
    map.insert(
        "prettier".to_string(),
        FormatterEntry {
            command: Some(vec![
                "npx".into(),
                "prettier".into(),
                "--write".into(),
                "$FILE".into(),
            ]),
            extensions: Some(vec![".ts".into()]),
            ..Default::default()
        },
    );
    let fmts = resolve_formatters(Some(&FormatterConfig::Map(map)));
    let p = fmts
        .iter()
        .find(|f| f.name == "prettier")
        .expect("prettier present");
    assert_eq!(p.extensions, vec![".ts".to_string()]);
    match &p.detect {
        Detect::Command { command } => assert!(command.contains(&"npx".to_string())),
        other => panic!("override should switch detection to Command, got {other:?}"),
    }
}

#[test]
fn an_incomplete_custom_entry_is_ignored() {
    // A custom name with a command but no extensions can't match anything → dropped.
    let mut map = BTreeMap::new();
    map.insert(
        "noext".to_string(),
        FormatterEntry {
            command: Some(vec!["x".into(), "$FILE".into()]),
            ..Default::default()
        },
    );
    let fmts = resolve_formatters(Some(&FormatterConfig::Map(map)));
    assert!(!fmts.iter().any(|f| f.name == "noext"));
}
