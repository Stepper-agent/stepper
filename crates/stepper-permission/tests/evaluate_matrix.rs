//! Integration matrix for the pure `deny > ask > allow > mode` engine, exercised
//! through the public `evaluate` entry point.

use std::path::{Path, PathBuf};
use stepper_permission::{
    Decision, PermissionMode, PermissionRequest, RuleSet,
};

fn root() -> PathBuf {
    PathBuf::from("/project")
}

fn judge(
    request: PermissionRequest,
    rules: &RuleSet,
    project_root: &Path,
    mode: PermissionMode,
) -> Decision {
    stepper_permission::evaluate(&request, rules, project_root, None, mode)
}

#[test]
fn relative_redirect_resolves_against_cwd_not_project_root() {
    use stepper_permission::evaluate_in;
    let rules = RuleSet::from_lists(&[], &[], &["Write(/sub/**)".into()]);
    let project = root();
    let cwd = project.join("sub");
    // A relative redirect run from cwd=/project/sub lands under /project/sub → denied.
    assert_eq!(
        evaluate_in(
            &PermissionRequest::Bash("echo x > out.txt".into()),
            &rules,
            &project,
            None,
            &cwd,
            PermissionMode::Auto,
        ),
        Decision::Deny,
        "the relative redirect anchors at cwd, hitting deny Write(/sub/**)"
    );
    // The same relative target from the project root is /project/out.txt → not denied.
    assert_ne!(
        evaluate_in(
            &PermissionRequest::Bash("echo x > out.txt".into()),
            &rules,
            &project,
            None,
            &project,
            PermissionMode::Auto,
        ),
        Decision::Deny,
        "from project root the redirect is outside /sub"
    );
    // An absolute target ignores cwd entirely (judged by its own path).
    assert_eq!(
        evaluate_in(
            &PermissionRequest::Bash("echo x > /project/sub/abs.txt".into()),
            &rules,
            &project,
            None,
            &project,
            PermissionMode::Auto,
        ),
        Decision::Deny,
        "absolute target under /sub is denied regardless of cwd"
    );
}

#[test]
fn deny_rule_beats_overlapping_allow_rule() {
    let rules = RuleSet::from_lists(
        &["Bash(rm *)".into()],
        &[],
        &["Bash(rm -rf *)".into()],
    );
    assert_eq!(
        judge(
            PermissionRequest::Bash("rm -rf /var/data".into()),
            &rules,
            &root(),
            PermissionMode::Auto,
        ),
        Decision::Deny
    );
    assert_eq!(
        judge(
            PermissionRequest::Bash("rm /tmp/scratch".into()),
            &rules,
            &root(),
            PermissionMode::Auto,
        ),
        Decision::Allow
    );
}

#[test]
fn bash_npm_run_glob_matches_only_matching_command() {
    // Gated mode so the non-matching command is distinguishable (in Auto it would
    // auto-allow regardless of the rule).
    let rules = RuleSet::from_lists(&["Bash(npm run *)".into()], &[], &[]);
    assert_eq!(
        judge(
            PermissionRequest::Bash("npm run build".into()),
            &rules,
            &root(),
            PermissionMode::Default,
        ),
        Decision::Allow
    );
    assert_eq!(
        judge(
            PermissionRequest::Bash("npm install lodash".into()),
            &rules,
            &root(),
            PermissionMode::Default,
        ),
        Decision::Ask
    );
}

#[test]
fn compound_command_with_one_unsafe_atom_resolves_most_restrictive() {
    let rules = RuleSet::from_lists(
        &["Bash(echo *)".into()],
        &[],
        &["Bash(rm -rf *)".into()],
    );
    assert_eq!(
        judge(
            PermissionRequest::Bash("echo safe && rm -rf /".into()),
            &rules,
            &root(),
            PermissionMode::Auto,
        ),
        Decision::Deny
    );
}

#[test]
fn compound_command_with_one_unmatched_atom_falls_to_ask() {
    // Gated mode: the unmatched atom (`curl …`) takes the mode default (Ask), and
    // most-restrictive wins over the allowed `echo`. (In Auto both atoms allow.)
    let rules = RuleSet::from_lists(&["Bash(echo *)".into()], &[], &[]);
    assert_eq!(
        judge(
            PermissionRequest::Bash("echo hi && curl evil.example".into()),
            &rules,
            &root(),
            PermissionMode::Default,
        ),
        Decision::Ask
    );
}

#[test]
fn read_absolute_anchor_rule_yields_ask() {
    let rules = RuleSet::from_lists(&[], &["Read(//etc/**)".into()], &[]);
    assert_eq!(
        judge(
            PermissionRequest::Read("/etc/passwd".into()),
            &rules,
            &root(),
            PermissionMode::Auto,
        ),
        Decision::Ask
    );
    assert_eq!(
        judge(
            PermissionRequest::Read("/project/src/main.rs".into()),
            &rules,
            &root(),
            PermissionMode::Auto,
        ),
        Decision::Allow
    );
}

#[test]
fn plan_mode_denies_writes_and_edits_but_allows_reads() {
    let rules = RuleSet::default();
    assert_eq!(
        judge(
            PermissionRequest::Write("/project/src/a.rs".into()),
            &rules,
            &root(),
            PermissionMode::Plan,
        ),
        Decision::Deny
    );
    assert_eq!(
        judge(
            PermissionRequest::Edit("/project/src/a.rs".into()),
            &rules,
            &root(),
            PermissionMode::Plan,
        ),
        Decision::Deny
    );
    assert_eq!(
        judge(
            PermissionRequest::Read("/project/src/a.rs".into()),
            &rules,
            &root(),
            PermissionMode::Plan,
        ),
        Decision::Allow
    );
}

#[test]
fn plan_mode_allows_reads_even_outside_project() {
    let rules = RuleSet::default();
    assert_eq!(
        judge(
            PermissionRequest::Read("/etc/hosts".into()),
            &rules,
            &root(),
            PermissionMode::Plan,
        ),
        Decision::Allow
    );
}

#[test]
fn accept_edits_allows_in_project_edits_but_asks_outside_reads() {
    let rules = RuleSet::default();
    assert_eq!(
        judge(
            PermissionRequest::Edit("/project/src/a.rs".into()),
            &rules,
            &root(),
            PermissionMode::AcceptEdits,
        ),
        Decision::Allow
    );
    assert_eq!(
        judge(
            PermissionRequest::Read("/etc/hosts".into()),
            &rules,
            &root(),
            PermissionMode::AcceptEdits,
        ),
        Decision::Ask
    );
}

#[test]
fn auto_mode_allows_in_project_and_asks_outside() {
    let rules = RuleSet::default();
    assert_eq!(
        judge(
            PermissionRequest::Write("/project/src/a.rs".into()),
            &rules,
            &root(),
            PermissionMode::Auto,
        ),
        Decision::Allow
    );
    assert_eq!(
        judge(
            PermissionRequest::Write("/etc/cron.d/job".into()),
            &rules,
            &root(),
            PermissionMode::Auto,
        ),
        Decision::Ask
    );
}

#[test]
fn auto_mode_auto_allows_bare_bash_without_rule() {
    // Auto is autonomous: an ordinary command needs no allow rule (deny rules,
    // redirect escalation, and secret screening remain the guardrails).
    let rules = RuleSet::default();
    assert_eq!(
        judge(
            PermissionRequest::Bash("ls -la".into()),
            &rules,
            &root(),
            PermissionMode::Auto,
        ),
        Decision::Allow
    );
}

#[test]
fn allow_rule_with_redirection_target_escalates_to_ask() {
    let rules = RuleSet::from_lists(&["Bash(echo *)".into()], &[], &[]);
    assert_eq!(
        judge(
            PermissionRequest::Bash("echo hello".into()),
            &rules,
            &root(),
            PermissionMode::Auto,
        ),
        Decision::Allow
    );
    assert_eq!(
        judge(
            PermissionRequest::Bash("echo x > /etc/passwd".into()),
            &rules,
            &root(),
            PermissionMode::Auto,
        ),
        Decision::Ask
    );
}

#[test]
fn allow_rule_with_unmatched_substitution_or_backtick_falls_to_ask() {
    // Gated mode: the inner command is its own atom; with no rule for it the mode
    // default (Ask) governs the compound. (In Auto the inner atom auto-allows, so
    // a deny rule — not the mode — is what gates it there.)
    let rules = RuleSet::from_lists(&["Bash(echo *)".into()], &[], &[]);
    assert_eq!(
        judge(
            PermissionRequest::Bash("echo $(rm -rf /)".into()),
            &rules,
            &root(),
            PermissionMode::Default,
        ),
        Decision::Ask
    );
    assert_eq!(
        judge(
            PermissionRequest::Bash("echo `whoami`".into()),
            &rules,
            &root(),
            PermissionMode::Default,
        ),
        Decision::Ask
    );
    // A trailing `&` only backgrounds the allowed command — nothing hides
    // behind it once the lone `&` is a separator.
    assert_eq!(
        judge(
            PermissionRequest::Bash("echo done &".into()),
            &rules,
            &root(),
            PermissionMode::Default,
        ),
        Decision::Allow
    );
}

#[test]
fn deny_rule_sees_command_after_lone_ampersand() {
    let rules = RuleSet::from_lists(&["Bash(echo *)".into()], &[], &["Bash(rm *)".into()]);
    assert_eq!(
        judge(
            PermissionRequest::Bash("echo a & rm -rf /".into()),
            &rules,
            &root(),
            PermissionMode::Auto,
        ),
        Decision::Deny
    );
}

#[test]
fn deny_rule_sees_command_inside_substitution_and_backticks() {
    let rules = RuleSet::from_lists(&["Bash(echo *)".into()], &[], &["Bash(rm *)".into()]);
    assert_eq!(
        judge(
            PermissionRequest::Bash("echo $(rm -rf /)".into()),
            &rules,
            &root(),
            PermissionMode::Auto,
        ),
        Decision::Deny
    );
    assert_eq!(
        judge(
            PermissionRequest::Bash("echo `rm -rf /`".into()),
            &rules,
            &root(),
            PermissionMode::Auto,
        ),
        Decision::Deny
    );
    assert_eq!(
        judge(
            PermissionRequest::Bash("echo \"$(rm -rf /)\"".into()),
            &rules,
            &root(),
            PermissionMode::Auto,
        ),
        Decision::Deny
    );
    assert_eq!(
        judge(
            PermissionRequest::Bash("diff <(rm -rf /) <(ls)".into()),
            &rules,
            &root(),
            PermissionMode::Auto,
        ),
        Decision::Deny
    );
}

#[test]
fn redirection_target_is_gated_as_a_path_request() {
    let rules = RuleSet::from_lists(
        &["Bash(echo *)".into(), "Bash(cat *)".into()],
        &[],
        &["Write(//etc/**)".into(), "Read(//secrets/**)".into()],
    );
    assert_eq!(
        judge(
            PermissionRequest::Bash("echo x > /etc/passwd".into()),
            &rules,
            &root(),
            PermissionMode::Auto,
        ),
        Decision::Deny
    );
    assert_eq!(
        judge(
            PermissionRequest::Bash("cat < /secrets/key".into()),
            &rules,
            &root(),
            PermissionMode::Auto,
        ),
        Decision::Deny
    );
    // An in-project redirection target is vetted like any other write and
    // auto-allowed by the mode, not blanket-escalated.
    assert_eq!(
        judge(
            PermissionRequest::Bash("echo x > notes.txt".into()),
            &rules,
            &root(),
            PermissionMode::Auto,
        ),
        Decision::Allow
    );
}

#[test]
fn undecomposable_command_fails_closed_to_deny() {
    let rules = RuleSet::from_lists(&["Bash(echo *)".into()], &[], &[]);
    for cmd in ["echo $(rm -rf /", "echo `pwd", "echo 'unterminated", "echo >"] {
        assert_eq!(
            judge(
                PermissionRequest::Bash(cmd.into()),
                &rules,
                &root(),
                PermissionMode::Auto,
            ),
            Decision::Deny,
            "expected fail-closed deny for: {cmd}"
        );
    }
    // Even bypass cannot run what the engine cannot analyze.
    assert_eq!(
        judge(
            PermissionRequest::Bash("echo $(rm -rf /".into()),
            &rules,
            &root(),
            PermissionMode::Bypass,
        ),
        Decision::Deny
    );
}

#[test]
fn quoted_redirection_metachar_does_not_escalate() {
    let rules = RuleSet::from_lists(&["Bash(echo *)".into()], &[], &[]);
    assert_eq!(
        judge(
            PermissionRequest::Bash("echo 'value > threshold'".into()),
            &rules,
            &root(),
            PermissionMode::Auto,
        ),
        Decision::Allow
    );
}

#[test]
fn deny_rule_with_redirection_stays_deny_not_downgraded_to_ask() {
    let rules = RuleSet::from_lists(
        &["Bash(echo *)".into()],
        &[],
        &["Bash(echo x*)".into()],
    );
    assert_eq!(
        judge(
            PermissionRequest::Bash("echo x > /etc/passwd".into()),
            &rules,
            &root(),
            PermissionMode::Auto,
        ),
        Decision::Deny
    );
}

#[test]
fn mcp_rule_allows_specific_server_tool_pair() {
    // Gated mode so the non-matching pairs are distinguishable (in Auto MCP
    // auto-allows regardless of the rule).
    let rules = RuleSet::from_lists(&["Mcp(filesystem, read_file)".into()], &[], &[]);
    assert_eq!(
        judge(
            PermissionRequest::Mcp {
                server: "filesystem".into(),
                tool: "read_file".into(),
            },
            &rules,
            &root(),
            PermissionMode::Default,
        ),
        Decision::Allow
    );
    assert_eq!(
        judge(
            PermissionRequest::Mcp {
                server: "filesystem".into(),
                tool: "write_file".into(),
            },
            &rules,
            &root(),
            PermissionMode::Default,
        ),
        Decision::Ask
    );
    assert_eq!(
        judge(
            PermissionRequest::Mcp {
                server: "other".into(),
                tool: "read_file".into(),
            },
            &rules,
            &root(),
            PermissionMode::Default,
        ),
        Decision::Ask
    );
}

#[test]
fn mcp_server_wildcard_rule_matches_any_tool() {
    let rules = RuleSet::from_lists(&["Mcp(filesystem)".into()], &[], &[]);
    assert_eq!(
        judge(
            PermissionRequest::Mcp {
                server: "filesystem".into(),
                tool: "anything".into(),
            },
            &rules,
            &root(),
            PermissionMode::Auto,
        ),
        Decision::Allow
    );
}

#[test]
fn mcp_deny_rule_overrides_allow_rule() {
    let rules = RuleSet::from_lists(
        &["Mcp(filesystem)".into()],
        &[],
        &["Mcp(filesystem, write_file)".into()],
    );
    assert_eq!(
        judge(
            PermissionRequest::Mcp {
                server: "filesystem".into(),
                tool: "write_file".into(),
            },
            &rules,
            &root(),
            PermissionMode::Auto,
        ),
        Decision::Deny
    );
}

#[test]
fn approvals_fold_into_allow_rules() {
    let rules = RuleSet::default().with_approvals(&["Bash(cargo *)".into()]);
    assert_eq!(
        judge(
            PermissionRequest::Bash("cargo test".into()),
            &rules,
            &root(),
            PermissionMode::Auto,
        ),
        Decision::Allow
    );
}

#[test]
fn empty_bash_command_falls_back_to_mode_default() {
    // Gated mode shows the fallback distinctly (Default's Ask); in Auto the
    // bash default is Allow.
    let rules = RuleSet::default();
    assert_eq!(
        judge(
            PermissionRequest::Bash("".into()),
            &rules,
            &root(),
            PermissionMode::Default,
        ),
        Decision::Ask
    );
}

#[test]
fn bash_npm_run_glob_does_not_over_match_run_prefix_without_space() {
    // Gated mode so the over-match cases are distinguishable from the allowed one.
    let rules = RuleSet::from_lists(&["Bash(npm run *)".into()], &[], &[]);
    assert_eq!(
        judge(
            PermissionRequest::Bash("npm run build".into()),
            &rules,
            &root(),
            PermissionMode::Default,
        ),
        Decision::Allow
    );
    assert_eq!(
        judge(
            PermissionRequest::Bash("npm runner".into()),
            &rules,
            &root(),
            PermissionMode::Default,
        ),
        Decision::Ask
    );
    assert_eq!(
        judge(
            PermissionRequest::Bash("npm running".into()),
            &rules,
            &root(),
            PermissionMode::Default,
        ),
        Decision::Ask
    );
}

#[test]
fn plan_mode_default_asks_bash_with_no_matching_rule() {
    let rules = RuleSet::default();
    assert_eq!(
        judge(
            PermissionRequest::Bash("ls -la".into()),
            &rules,
            &root(),
            PermissionMode::Plan,
        ),
        Decision::Ask
    );
}

#[test]
fn accept_edits_mode_default_asks_bash_with_no_matching_rule() {
    let rules = RuleSet::default();
    assert_eq!(
        judge(
            PermissionRequest::Bash("ls -la".into()),
            &rules,
            &root(),
            PermissionMode::AcceptEdits,
        ),
        Decision::Ask
    );
}

#[test]
fn web_fetch_default_asks_without_rule_in_gated_modes() {
    // Auto auto-allows WebFetch now (web_fetch keeps its own SSRF guard); the
    // gated modes still ask without an explicit rule.
    let rules = RuleSet::default();
    for mode in [
        PermissionMode::Default,
        PermissionMode::Plan,
        PermissionMode::AcceptEdits,
    ] {
        assert_eq!(
            judge(
                PermissionRequest::WebFetch("https://example.com".into()),
                &rules,
                &root(),
                mode,
            ),
            Decision::Ask
        );
    }
    // … and Auto auto-allows it.
    assert_eq!(
        judge(
            PermissionRequest::WebFetch("https://example.com".into()),
            &rules,
            &root(),
            PermissionMode::Auto,
        ),
        Decision::Allow
    );
}

#[test]
fn web_fetch_allow_rule_matches_host_glob() {
    let rules = RuleSet::from_lists(&["WebFetch(https://example.com/*)".into()], &[], &[]);
    assert_eq!(
        judge(
            PermissionRequest::WebFetch("https://example.com/data.json".into()),
            &rules,
            &root(),
            PermissionMode::Plan,
        ),
        Decision::Allow
    );
    assert_eq!(
        judge(
            PermissionRequest::WebFetch("https://evil.example/data.json".into()),
            &rules,
            &root(),
            PermissionMode::Plan,
        ),
        Decision::Ask
    );
}

#[test]
fn web_fetch_deny_rule_overrides_allow_rule() {
    let rules = RuleSet::from_lists(
        &["WebFetch(https://*)".into()],
        &[],
        &["WebFetch(https://evil.example/*)".into()],
    );
    assert_eq!(
        judge(
            PermissionRequest::WebFetch("https://evil.example/x".into()),
            &rules,
            &root(),
            PermissionMode::Auto,
        ),
        Decision::Deny
    );
}

#[test]
fn other_request_default_asks_without_rule() {
    // Gated mode (Auto auto-allows like shell/fetch/mcp).
    let rules = RuleSet::default();
    assert_eq!(
        judge(
            PermissionRequest::Other {
                tool: "TodoWrite".into(),
                arg: "anything".into(),
            },
            &rules,
            &root(),
            PermissionMode::Default,
        ),
        Decision::Ask
    );
}

#[test]
fn other_request_matches_named_tool_rule_and_arg_glob() {
    let rules = RuleSet::from_lists(&["TodoWrite(plan-*)".into()], &[], &[]);
    assert_eq!(
        judge(
            PermissionRequest::Other {
                tool: "TodoWrite".into(),
                arg: "plan-step-1".into(),
            },
            &rules,
            &root(),
            PermissionMode::Plan,
        ),
        Decision::Allow
    );
    assert_eq!(
        judge(
            PermissionRequest::Other {
                tool: "TodoWrite".into(),
                arg: "other".into(),
            },
            &rules,
            &root(),
            PermissionMode::Plan,
        ),
        Decision::Ask
    );
    assert_eq!(
        judge(
            PermissionRequest::Other {
                tool: "OtherTool".into(),
                arg: "plan-step-1".into(),
            },
            &rules,
            &root(),
            PermissionMode::Plan,
        ),
        Decision::Ask
    );
}

#[test]
fn default_mode_allows_reads_and_asks_everything_else() {
    let rules = RuleSet::default();
    assert_eq!(
        judge(
            PermissionRequest::Read("/project/src/a.rs".into()),
            &rules,
            &root(),
            PermissionMode::Default,
        ),
        Decision::Allow
    );
    assert_eq!(
        judge(
            PermissionRequest::Read("/etc/hosts".into()),
            &rules,
            &root(),
            PermissionMode::Default,
        ),
        Decision::Allow
    );
    for request in [
        PermissionRequest::Write("/project/src/a.rs".into()),
        PermissionRequest::Edit("/project/src/a.rs".into()),
        PermissionRequest::Bash("ls -la".into()),
        PermissionRequest::WebFetch("https://example.com".into()),
    ] {
        assert_eq!(
            judge(request, &rules, &root(), PermissionMode::Default),
            Decision::Ask
        );
    }
}

#[test]
fn dont_ask_mode_auto_denies_anything_that_would_ask() {
    let rules = RuleSet::from_lists(&["Bash(cargo *)".into()], &["Bash(git push:*)".into()], &[]);
    // Reads stay allowed, explicit allow rules stay allowed.
    assert_eq!(
        judge(
            PermissionRequest::Read("/project/src/a.rs".into()),
            &rules,
            &root(),
            PermissionMode::DontAsk,
        ),
        Decision::Allow
    );
    assert_eq!(
        judge(
            PermissionRequest::Bash("cargo build".into()),
            &rules,
            &root(),
            PermissionMode::DontAsk,
        ),
        Decision::Allow
    );
    // Everything that would prompt is denied instead (CI-safe).
    for request in [
        PermissionRequest::Write("/project/src/a.rs".into()),
        PermissionRequest::Bash("ls -la".into()),
        PermissionRequest::Bash("git push origin main".into()),
        PermissionRequest::WebFetch("https://example.com".into()),
    ] {
        assert_eq!(
            judge(request, &rules, &root(), PermissionMode::DontAsk),
            Decision::Deny
        );
    }
}

#[test]
fn bypass_mode_allows_anything_that_would_ask_but_deny_still_denies() {
    let rules = RuleSet::from_lists(
        &[],
        &["Bash(git push:*)".into()],
        &["Bash(rm *)".into(), "Write(//etc/**)".into()],
    );
    for request in [
        PermissionRequest::Write("/project/src/a.rs".into()),
        PermissionRequest::Write("/opt/outside.txt".into()),
        PermissionRequest::Bash("ls -la".into()),
        PermissionRequest::Bash("git push origin main".into()),
        PermissionRequest::WebFetch("https://example.com".into()),
    ] {
        assert_eq!(
            judge(request, &rules, &root(), PermissionMode::Bypass),
            Decision::Allow
        );
    }
    for request in [
        PermissionRequest::Bash("rm -rf /tmp/x".into()),
        PermissionRequest::Write("/etc/passwd".into()),
        PermissionRequest::Bash("echo a && rm -rf /".into()),
    ] {
        assert_eq!(
            judge(request, &rules, &root(), PermissionMode::Bypass),
            Decision::Deny
        );
    }
}

#[test]
fn single_star_path_rule_stays_within_one_segment() {
    use stepper_permission::path;
    let root = root();
    assert!(path::path_matches("/src/*", Path::new("/project/src/a.rs"), &root, None));
    assert!(!path::path_matches(
        "/src/*",
        Path::new("/project/src/deep/nested.rs"),
        &root,
        None
    ));
    assert!(path::path_matches(
        "/src/**",
        Path::new("/project/src/deep/nested.rs"),
        &root,
        None
    ));
    // The scaffold `Read(/**)` stays recursive over the whole project.
    assert!(path::path_matches("/**", Path::new("/project/a/b/c.rs"), &root, None));
}

#[test]
fn from_lists_checked_rejects_malformed_deny_and_warns_on_allow() {
    let err = RuleSet::from_lists_checked(&[], &[], &["Bash(rm *".into()]).unwrap_err();
    assert!(
        err.to_string().contains("Bash(rm *"),
        "the error names the malformed deny spec: {err}"
    );

    let (rules, dropped) = RuleSet::from_lists_checked(
        &["Bash(cargo *)".into(), "Bash(broken".into()],
        &["(no-tool)".into()],
        &["Bash(rm *)".into()],
    )
    .unwrap();
    assert_eq!(rules.allow.len(), 1);
    assert_eq!(rules.ask.len(), 0);
    assert_eq!(rules.deny.len(), 1);
    assert_eq!(dropped, vec!["Bash(broken".to_string(), "(no-tool)".to_string()]);
}

#[test]
fn layer_extended_rules_tighten_the_base() {
    let base = RuleSet::from_lists(&["Bash(*)".into()], &[], &[]);
    let layer = base.extended(&[], &[], &["Bash(rm *)".into()]);
    // The base alone would allow `rm`; the layer's deny override wins.
    assert_eq!(
        judge(
            PermissionRequest::Bash("rm -rf /tmp".into()),
            &layer,
            &root(),
            PermissionMode::Auto,
        ),
        Decision::Deny
    );
    // A non-rm command is still allowed by the inherited base allow.
    assert_eq!(
        judge(
            PermissionRequest::Bash("ls -la".into()),
            &layer,
            &root(),
            PermissionMode::Auto,
        ),
        Decision::Allow
    );
    // `extended` is non-mutating: the base set is unchanged.
    assert_eq!(
        judge(
            PermissionRequest::Bash("rm -rf /tmp".into()),
            &base,
            &root(),
            PermissionMode::Auto,
        ),
        Decision::Allow
    );
}
