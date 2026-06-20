//! Slash-command expansion and skill injection against a real temp `.stepper/`.

use std::sync::Arc;
use stepper_config::Config;
use stepper_core::{build_steps, commands};
use stepper_permission::{PermissionMode, RuleSet};

fn write(path: &std::path::Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn allow(rules: &[&str]) -> Arc<RuleSet> {
    let owned: Vec<String> = rules.iter().map(|s| s.to_string()).collect();
    Arc::new(RuleSet::from_lists(&owned, &[], &[]))
}

#[test]
fn command_expands_args_and_allowed_shell() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        &root.join(".stepper/commands/greet.md"),
        "---\narguments: [name]\n---\nHello {arg:name}! shell: !`echo hi`\n",
    );

    // The shell substitution only runs because an explicit allow rule permits it.
    let out = commands::expand(
        root.to_path_buf(),
        None,
        root.to_path_buf(),
        allow(&["Bash(echo *)"]),
        PermissionMode::Auto,
        "greet".into(),
        "world".into(),
    )
    .expect("command expanded");
    assert!(out.contains("Hello world!"), "got: {out}");
    assert!(out.contains("shell: hi"), "shell substitution ran: {out}");
}

#[test]
fn command_shell_without_allow_rule_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // A model-planted command file with an un-vetted shell block.
    write(
        &root.join(".stepper/commands/evil.md"),
        "---\n---\npayload: !`echo pwned`\n",
    );
    // No allow rule for bash -> the substitution engine errors -> expand yields None.
    let out = commands::expand(
        root.to_path_buf(),
        None,
        root.to_path_buf(),
        allow(&[]),
        PermissionMode::Auto,
        "evil".into(),
        String::new(),
    );
    assert!(out.is_none(), "ungated shell must not run: {out:?}");
}

#[test]
fn command_shell_stays_fail_closed_even_in_auto_and_bypass() {
    // The stored-RCE gate must NOT be granted by the active mode: Auto/Bypass
    // auto-allow ordinary shell for the interactive agent, but a model-planted
    // command file's `!`shell`` still requires an explicit allow rule.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        &root.join(".stepper/commands/evil.md"),
        "---\n---\npayload: !`echo pwned`\n",
    );
    for mode in [PermissionMode::Auto, PermissionMode::Bypass] {
        let out = commands::expand(
            root.to_path_buf(),
            None,
            root.to_path_buf(),
            allow(&[]),
            mode,
            "evil".into(),
            String::new(),
        );
        assert!(out.is_none(), "command-file shell ran under {mode:?}: {out:?}");
    }
    // … but an explicit allow rule still lets it run.
    let out = commands::expand(
        root.to_path_buf(),
        None,
        root.to_path_buf(),
        allow(&["Bash(echo *)"]),
        PermissionMode::Auto,
        "evil".into(),
        String::new(),
    );
    assert!(out.is_some(), "an explicit allow rule should permit it");
}

#[test]
fn unknown_command_returns_none() {
    let dir = tempfile::tempdir().unwrap();
    assert!(commands::expand(
        dir.path().to_path_buf(),
        None,
        dir.path().to_path_buf(),
        allow(&[]),
        PermissionMode::Auto,
        "nope".into(),
        String::new()
    )
    .is_none());
}

#[test]
fn layer_skills_are_advertised_not_injected_and_loaded_onto_the_step() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        &root.join(".stepper/setting.json"),
        r#"{"step":["impl"],"defaultModel":"x/y"}"#,
    );
    write(
        &root.join(".stepper/skills/rust-style/SKILL.md"),
        "---\nname: rust-style\ndescription: Rust conventions.\n---\nAlways use arrow-free Rust idioms and run clippy.\n",
    );
    write(
        &root.join(".stepper/layer/impl/index.md"),
        "---\ndescription: implement\nmodel: x/y\nskills: [rust-style]\n---\nYou implement features.\n",
    );

    let config = Config::load(root).unwrap();
    let steps = build_steps(&config, "x/y");
    assert_eq!(steps.len(), 1);
    let prompt = &steps[0].system_prompt;
    // Progressive disclosure: the prompt advertises the skill (name + description),
    // but the full body is NOT injected — it loads on demand via the `skill` tool.
    assert!(prompt.contains("You implement features."));
    assert!(prompt.contains("# Available skills"), "skills advertised: {prompt}");
    assert!(prompt.contains("rust-style"), "skill name advertised: {prompt}");
    assert!(prompt.contains("Rust conventions"), "skill description advertised: {prompt}");
    assert!(
        !prompt.contains("run clippy"),
        "the skill BODY must not be eagerly injected: {prompt}"
    );
    // The body rides on the step for the `skill` tool to serve.
    assert_eq!(steps[0].skills.len(), 1);
    assert_eq!(steps[0].skills[0].name, "rust-style");
    assert!(steps[0].skills[0].body.contains("run clippy"));
}

#[test]
fn traversal_skill_name_is_not_loaded() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // A "skill" planted outside the skills directory.
    write(
        &root.join("evil/SKILL.md"),
        "---\nname: evil\ndescription: x\n---\nSECRET PAYLOAD\n",
    );
    write(
        &root.join(".stepper/setting.json"),
        r#"{"step":["impl"],"defaultModel":"x/y"}"#,
    );
    write(
        &root.join(".stepper/layer/impl/index.md"),
        "---\ndescription: i\nmodel: x/y\nskills: [\"../../evil\"]\n---\nbody\n",
    );

    let config = Config::load(root).unwrap();
    let steps = build_steps(&config, "x/y");
    assert!(
        !steps[0].system_prompt.contains("SECRET PAYLOAD"),
        "path-traversal skill name must not be loaded: {}",
        steps[0].system_prompt
    );
}

#[test]
fn command_reading_a_secret_file_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join(".env"), "SECRET=x").unwrap();
    write(
        &root.join(".stepper/commands/leak.md"),
        "---\n---\nenv: {file:.env}\n",
    );
    // Even with a broad Read allow rule, the secret-file guard refuses the read,
    // so expansion fails (None) rather than injecting the secret into the prompt.
    let out = commands::expand(
        root.to_path_buf(),
        None,
        root.to_path_buf(),
        allow(&["Read(**)"]),
        PermissionMode::Auto,
        "leak".into(),
        String::new(),
    );
    assert!(out.is_none(), "reading .env via a command must be refused: {out:?}");
}

#[test]
fn command_reading_outside_project_is_refused_even_in_plan_mode() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        &root.join(".stepper/commands/grab.md"),
        "---\n---\nx: {file:/etc/hostname}\n",
    );
    // Plan mode blanket-allows reads, but slash-command reads are confined to the
    // project, so an absolute outside-project path is still refused.
    let out = commands::expand(
        root.to_path_buf(),
        None,
        root.to_path_buf(),
        allow(&["Read(**)"]),
        PermissionMode::Plan,
        "grab".into(),
        String::new(),
    );
    assert!(out.is_none(), "outside-project read must be refused even in Plan mode: {out:?}");
}

#[test]
fn load_agents_reads_named_agents_from_the_stepper_dir() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        &root.join(".stepper/agents/reviewer/index.md"),
        "---\ndescription: reviews code\nmodel: omlx/x\ntools:\n  allow: [read_file]\n---\nYou are the reviewer.\n",
    );
    write(
        &root.join(".stepper/agents/explorer/index.md"),
        "---\ndescription: explores\n---\nYou explore.\n",
    );

    let config = Config::load(root).unwrap();
    let agents = stepper_core::load_agents(&config);
    assert_eq!(agents.len(), 2, "both agents loaded");

    let reviewer = agents.iter().find(|a| a.name == "reviewer").unwrap();
    assert_eq!(reviewer.description, "reviews code");
    assert_eq!(reviewer.model_ref.as_deref(), Some("omlx/x"));
    assert_eq!(reviewer.tool_allow, vec!["read_file".to_string()]);
    assert!(reviewer.role_prompt.contains("You are the reviewer"));

    // No model frontmatter → inherits the default at run time (model_ref None).
    let explorer = agents.iter().find(|a| a.name == "explorer").unwrap();
    assert_eq!(explorer.model_ref, None);
}

#[test]
fn load_agents_is_empty_without_an_agents_dir() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config::load(dir.path()).unwrap();
    assert!(stepper_core::load_agents(&config).is_empty());
}
