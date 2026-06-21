//! `stepper-permission` — the pure `deny > ask > allow` decision engine.
//!
//! `evaluate` is a pure function: rule matching with path anchors
//! (`/`·`//`·`~/`, symlink-resolved), compound-bash gating (most-restrictive
//! wins), and mode defaults. Persisting an `always-allow` is the caller's job
//! (it is the only IO, handled in config/core).

pub mod bash;
pub mod path;
pub mod request;
pub mod rule;

pub use request::{Decision, PermissionMode, PermissionRequest};
pub use rule::{MatchTarget, Rule};

use std::path::{Path, PathBuf};
use thiserror::Error;

/// Rule-list parsing failures that must stop startup: an unparseable DENY spec
/// silently dropping would fail open (the user believes the protection exists).
#[derive(Debug, Error)]
pub enum RuleSetError {
    #[error("malformed deny rule(s): {}", specs.join(", "))]
    MalformedDeny { specs: Vec<String> },
}

/// The active rules, split by verdict. Persisted `approvals` are folded into
/// `allow`.
#[derive(Debug, Clone, Default)]
pub struct RuleSet {
    pub allow: Vec<Rule>,
    pub ask: Vec<Rule>,
    pub deny: Vec<Rule>,
    /// Extra roots (config `permissions.additionalDirectories`) treated as
    /// in-project for mode defaults, so reads/writes under them are not
    /// escalated as out-of-project.
    pub additional_dirs: Vec<std::path::PathBuf>,
}

impl RuleSet {
    pub fn from_lists(allow: &[String], ask: &[String], deny: &[String]) -> Self {
        RuleSet {
            allow: rule::parse_all(allow),
            ask: rule::parse_all(ask),
            deny: rule::parse_all(deny),
            additional_dirs: Vec::new(),
        }
    }

    /// Set the extra in-project roots (`additionalDirectories`).
    pub fn with_additional_dirs(mut self, dirs: Vec<std::path::PathBuf>) -> Self {
        self.additional_dirs = dirs;
        self
    }

    /// Like `from_lists`, but malformed specs are surfaced instead of silently
    /// dropped: a malformed deny is a hard error (fail closed), malformed
    /// allow/ask specs come back as warnings for the caller to report.
    pub fn from_lists_checked(
        allow: &[String],
        ask: &[String],
        deny: &[String],
    ) -> Result<(RuleSet, Vec<String>), RuleSetError> {
        let (deny_rules, malformed_deny) = rule::parse_all_checked(deny);
        if !malformed_deny.is_empty() {
            return Err(RuleSetError::MalformedDeny {
                specs: malformed_deny,
            });
        }
        let (allow_rules, mut dropped) = rule::parse_all_checked(allow);
        let (ask_rules, dropped_ask) = rule::parse_all_checked(ask);
        dropped.extend(dropped_ask);
        Ok((
            RuleSet {
                allow: allow_rules,
                ask: ask_rules,
                deny: deny_rules,
                additional_dirs: Vec::new(),
            },
            dropped,
        ))
    }

    /// Fold persisted always-allow approvals in as additional allow rules.
    pub fn with_approvals(mut self, approvals: &[String]) -> Self {
        self.allow.extend(rule::parse_all(approvals));
        self
    }

    /// A new rule set = these rules plus a layer's overrides. Since `evaluate`
    /// resolves `deny > ask > allow`, a layer can only tighten (add ask/deny) —
    /// it cannot relax a base `deny`.
    pub fn extended(&self, allow: &[String], ask: &[String], deny: &[String]) -> RuleSet {
        let mut r = self.clone();
        r.allow.extend(rule::parse_all(allow));
        r.ask.extend(rule::parse_all(ask));
        r.deny.extend(rule::parse_all(deny));
        r
    }
}

/// Evaluate a request. Explicit `deny` always wins; otherwise `ask` over
/// `allow`; otherwise the mode default. A compound bash command is decomposed
/// (operators, substitutions, redirection targets) and gated per-component with
/// the most restrictive verdict returned; an undecomposable command is denied
/// (fail closed). `DontAsk` turns the final `Ask` into `Deny`, `Bypass` turns it
/// into `Allow` — an explicit `deny` rule wins in every mode.
pub fn evaluate(
    request: &PermissionRequest,
    rules: &RuleSet,
    project_root: &Path,
    home: Option<&Path>,
    mode: PermissionMode,
) -> Decision {
    // Default the effective cwd to project_root (relative paths anchor there, the
    // historical behavior). Callers with a distinct working directory use
    // `evaluate_in` so bash redirect targets resolve against the real cwd.
    evaluate_in(request, rules, project_root, home, project_root, mode)
}

/// Like [`evaluate`], but resolves relative request/redirect paths against
/// `cwd` (the bash tool's effective working directory) instead of project_root.
pub fn evaluate_in(
    request: &PermissionRequest,
    rules: &RuleSet,
    project_root: &Path,
    home: Option<&Path>,
    cwd: &Path,
    mode: PermissionMode,
) -> Decision {
    let decision = evaluate_inner(request, rules, project_root, home, cwd, mode);
    match (mode, decision) {
        (PermissionMode::DontAsk, Decision::Ask) => Decision::Deny,
        (PermissionMode::Bypass, Decision::Ask) => Decision::Allow,
        (_, decision) => decision,
    }
}

fn evaluate_inner(
    request: &PermissionRequest,
    rules: &RuleSet,
    project_root: &Path,
    home: Option<&Path>,
    cwd: &Path,
    mode: PermissionMode,
) -> Decision {
    match request {
        PermissionRequest::Bash(command) => {
            let Some(mut atoms) = bash::decompose(command) else {
                // Not fully analyzable (unbalanced quote/paren, dangling
                // redirection) — a deny rule could be hiding inside, fail closed.
                return Decision::Deny;
            };
            if atoms.is_empty() {
                atoms.push(bash::BashAtom {
                    command: command.clone(),
                    reads: Vec::new(),
                    writes: Vec::new(),
                    escalate: false,
                });
            }
            atoms
                .iter()
                .map(|atom| {
                    let decision = decide(
                        "Bash",
                        &MatchTarget::Command(&atom.command),
                        rules,
                        project_root,
                        home,
                        || mode_default_bash(mode),
                    );
                    // A command rule must not auto-allow a redirection whose
                    // target could not be analyzed — escalate Allow to Ask.
                    let decision = if decision == Decision::Allow && atom.escalate {
                        Decision::Ask
                    } else {
                        decision
                    };
                    // Analyzed redirection targets are gated as their own
                    // Read/Write path requests, so path deny rules see them.
                    atom.reads
                        .iter()
                        .map(|p| PermissionRequest::Read(PathBuf::from(p)))
                        .chain(
                            atom.writes
                                .iter()
                                .map(|p| PermissionRequest::Write(PathBuf::from(p))),
                        )
                        .fold(decision, |acc, target| {
                            acc.restrict(evaluate_inner(
                                &target,
                                rules,
                                project_root,
                                home,
                                cwd,
                                mode,
                            ))
                        })
                })
                .fold(Decision::Allow, Decision::restrict)
        }
        PermissionRequest::Read(p)
        | PermissionRequest::Write(p)
        | PermissionRequest::Edit(p) => {
            // Anchor a relative request/redirect path at the effective cwd before
            // matching, so `> out.txt` from a subdir is judged there; rule
            // patterns stay project_root-anchored inside `decide`/`is_in_project`.
            let anchored = path::anchor_at_cwd(p, cwd);
            // `additionalDirectories` count as in-project for mode defaults, so a
            // read/write under a configured extra root is not escalated as
            // out-of-project.
            let in_project = path::is_in_project(&anchored, project_root)
                || rules
                    .additional_dirs
                    .iter()
                    .any(|d| path::is_in_project(&anchored, d));
            let decision = decide(
                request.tool(),
                &MatchTarget::Path(&anchored),
                rules,
                project_root,
                home,
                || mode_default_path(request, mode, in_project),
            );
            // The `.stepper/` config dir (commands, hooks, settings) gates the
            // agent's own security — a write/edit there must be explicitly
            // confirmed, never silently auto-allowed by mode OR a broad rule. This
            // fires for any non-deny decision (an `Allow` from a broad rule, or an
            // `Ask` from the mode default), because in Bypass the top-level
            // transform would otherwise turn that `Ask` straight back into
            // `Allow`. An explicit `deny` still wins (checked first in `decide`).
            if !request.is_read_only()
                && decision != Decision::Deny
                && path::is_protected(&anchored, project_root)
            {
                // Bypass has no human to confirm — fail closed (Deny) rather than
                // auto-allow a write to the agent's own security config. Other
                // modes escalate to an explicit prompt (which DontAsk then denies).
                if mode == PermissionMode::Bypass {
                    Decision::Deny
                } else {
                    Decision::Ask
                }
            } else {
                decision
            }
        }
        PermissionRequest::WebFetch(url) => decide(
            "WebFetch",
            &MatchTarget::Text(url),
            rules,
            project_root,
            home,
            || mode_default_other(mode),
        ),
        PermissionRequest::Mcp { server, tool } => decide(
            "Mcp",
            &MatchTarget::Mcp { server, tool },
            rules,
            project_root,
            home,
            || mode_default_other(mode),
        ),
        PermissionRequest::Other { tool, arg } => decide(
            tool,
            &MatchTarget::Text(arg),
            rules,
            project_root,
            home,
            || mode_default_other(mode),
        ),
    }
}

fn decide(
    tool: &str,
    target: &MatchTarget,
    rules: &RuleSet,
    project_root: &Path,
    home: Option<&Path>,
    default: impl Fn() -> Decision,
) -> Decision {
    if rules.deny.iter().any(|r| r.matches(tool, target, project_root, home)) {
        return Decision::Deny;
    }
    if rules.ask.iter().any(|r| r.matches(tool, target, project_root, home)) {
        return Decision::Ask;
    }
    if rules.allow.iter().any(|r| r.matches(tool, target, project_root, home)) {
        return Decision::Allow;
    }
    default()
}

fn mode_default_bash(mode: PermissionMode) -> Decision {
    match mode {
        // Auto = autonomous: the user opted into not being nagged for ordinary
        // in-project work, so the per-atom default is Allow. The guardrails that
        // still fire keep this from being a blanket bypass: an explicit `deny`
        // rule wins first (decide()), an unanalyzable redirection/substitution
        // escalates Allow→Ask (evaluate_inner), an out-of-project redirect target
        // is gated as its own Write/Read and asks, and secret files are refused at
        // execution (the bash tool's secret screen). A robust default deny list
        // (scaffolded into .stepper/) is what stops a destructive verb with no
        // redirect (e.g. `rm -rf`).
        PermissionMode::Auto => Decision::Allow,
        // Every other mode keeps shell gated unless an explicit allow rule matches
        // (Bypass turns the resulting Ask into Allow at the top level, so the
        // redirect/secret guards above still apply there).
        PermissionMode::Plan
        | PermissionMode::AcceptEdits
        | PermissionMode::Default
        | PermissionMode::DontAsk
        | PermissionMode::Bypass => Decision::Ask,
    }
}

fn mode_default_path(
    request: &PermissionRequest,
    mode: PermissionMode,
    in_project: bool,
) -> Decision {
    let read_only = request.is_read_only();
    match mode {
        PermissionMode::Plan => {
            if read_only {
                Decision::Allow
            } else {
                Decision::Deny
            }
        }
        PermissionMode::Auto => {
            // Read-only tools (read/grep/glob/list) are auto-approved anywhere,
            // including outside the project — a read can't damage the working
            // tree, and secret files are still refused at the tool layer. Only
            // mutating tools (write/edit) are gated by project boundary: allowed
            // in-project, asked outside.
            if read_only || in_project {
                Decision::Allow
            } else {
                Decision::Ask
            }
        }
        PermissionMode::AcceptEdits => {
            // In-project reads and edits are auto-approved; anything outside the
            // project (read or write) still asks.
            if in_project {
                Decision::Allow
            } else {
                Decision::Ask
            }
        }
        // `default` allows read-only without a prompt and asks for everything
        // else; `dont-ask`/`bypass` share that base and the top-level transform
        // turns the Ask into Deny/Allow respectively.
        PermissionMode::Default | PermissionMode::DontAsk | PermissionMode::Bypass => {
            if read_only {
                Decision::Allow
            } else {
                Decision::Ask
            }
        }
    }
}

fn mode_default_other(mode: PermissionMode) -> Decision {
    // WebFetch / MCP follow the same posture as shell: Auto auto-allows (deny
    // rules still win, web_fetch keeps its own SSRF guard), every other mode asks
    // unless an explicit allow rule matches.
    match mode {
        PermissionMode::Auto => Decision::Allow,
        _ => Decision::Ask,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn root() -> PathBuf {
        PathBuf::from("/project")
    }

    #[test]
    fn deny_beats_allow_and_ask() {
        let rules = RuleSet::from_lists(
            &["Bash(rm *)".into()],
            &[],
            &["Bash(rm -rf *)".into()],
        );
        // Both allow(rm *) and deny(rm -rf *) match; deny wins.
        let d = evaluate(
            &PermissionRequest::Bash("rm -rf /tmp/x".into()),
            &rules,
            &root(),
            None,
            PermissionMode::Auto,
        );
        assert_eq!(d, Decision::Deny);
    }

    #[test]
    fn compound_bash_takes_most_restrictive() {
        let rules = RuleSet::from_lists(
            &["Bash(cargo *)".into()],
            &[],
            &["Bash(rm *)".into()],
        );
        // "cargo build" allowed, "rm -rf /" denied -> whole compound denied.
        let d = evaluate(
            &PermissionRequest::Bash("cargo build && rm -rf /".into()),
            &rules,
            &root(),
            None,
            PermissionMode::Auto,
        );
        assert_eq!(d, Decision::Deny);
    }

    #[test]
    fn ask_beats_allow() {
        let rules = RuleSet::from_lists(
            &["Bash(git *)".into()],
            &["Bash(git push:*)".into()],
            &[],
        );
        let d = evaluate(
            &PermissionRequest::Bash("git push origin main".into()),
            &rules,
            &root(),
            None,
            PermissionMode::Auto,
        );
        assert_eq!(d, Decision::Ask);
    }

    #[test]
    fn plan_mode_denies_writes_allows_reads() {
        let rules = RuleSet::default();
        assert_eq!(
            evaluate(
                &PermissionRequest::Write("/project/src/a.rs".into()),
                &rules,
                &root(),
                None,
                PermissionMode::Plan,
            ),
            Decision::Deny
        );
        assert_eq!(
            evaluate(
                &PermissionRequest::Read("/project/src/a.rs".into()),
                &rules,
                &root(),
                None,
                PermissionMode::Plan,
            ),
            Decision::Allow
        );
    }

    #[test]
    fn accept_edits_allows_in_project_asks_outside() {
        let rules = RuleSet::default();
        assert_eq!(
            evaluate(
                &PermissionRequest::Edit("/project/src/a.rs".into()),
                &rules,
                &root(),
                None,
                PermissionMode::AcceptEdits,
            ),
            Decision::Allow
        );
        assert_eq!(
            evaluate(
                &PermissionRequest::Edit("/etc/hosts".into()),
                &rules,
                &root(),
                None,
                PermissionMode::AcceptEdits,
            ),
            Decision::Ask
        );
    }

    #[test]
    fn redirection_and_substitution_escalate_allow_to_ask() {
        // A gated mode (Default): the per-atom default is Ask, so the gating
        // mechanisms below are visible. (In Auto the per-atom default is Allow —
        // see `auto_mode_*` — so the inner substitution command auto-allows there.)
        let rules = RuleSet::from_lists(&["Bash(echo *)".into()], &[], &[]);
        // plain allowed command stays allowed
        assert_eq!(
            evaluate(&PermissionRequest::Bash("echo hi".into()), &rules, &root(), None, PermissionMode::Default),
            Decision::Allow
        );
        // redirection to an arbitrary target must not auto-allow
        assert_eq!(
            evaluate(&PermissionRequest::Bash("echo x > /etc/passwd".into()), &rules, &root(), None, PermissionMode::Default),
            Decision::Ask
        );
        // command substitution must not auto-allow
        assert_eq!(
            evaluate(&PermissionRequest::Bash("echo $(rm -rf /)".into()), &rules, &root(), None, PermissionMode::Default),
            Decision::Ask
        );
        // but a quoted '>' is not a redirection
        assert_eq!(
            evaluate(&PermissionRequest::Bash("echo 'a > b'".into()), &rules, &root(), None, PermissionMode::Default),
            Decision::Allow
        );
    }

    #[test]
    fn writes_into_dot_stepper_escalate_to_ask_even_in_accept_edits() {
        let rules = RuleSet::default();
        // An in-project edit normally auto-allows in accept-edits…
        assert_eq!(
            evaluate(
                &PermissionRequest::Edit("/project/src/a.rs".into()),
                &rules,
                &root(),
                None,
                PermissionMode::AcceptEdits,
            ),
            Decision::Allow
        );
        // …but a write/edit under `.stepper/` (commands/hooks/settings) must ask.
        assert_eq!(
            evaluate(
                &PermissionRequest::Write("/project/.stepper/commands/x.md".into()),
                &rules,
                &root(),
                None,
                PermissionMode::AcceptEdits,
            ),
            Decision::Ask
        );
        // even a broad allow rule cannot silently auto-approve a `.stepper/` write.
        let broad = RuleSet::from_lists(&["Write(/**)".into()], &[], &[]);
        assert_eq!(
            evaluate(
                &PermissionRequest::Write("/project/.stepper/setting.json".into()),
                &broad,
                &root(),
                None,
                PermissionMode::Auto,
            ),
            Decision::Ask
        );
        // reading `.stepper/` is fine (only writes/edits are gated).
        assert_eq!(
            evaluate(
                &PermissionRequest::Read("/project/.stepper/setting.json".into()),
                &rules,
                &root(),
                None,
                PermissionMode::AcceptEdits,
            ),
            Decision::Allow
        );
    }

    #[test]
    fn dot_stepper_write_is_denied_in_bypass_not_auto_allowed() {
        // Bypass has no human to confirm, and the top-level (Bypass, Ask) => Allow
        // transform would otherwise auto-allow a `.stepper/` write. It must fail
        // closed (Deny) — both with a broad allow rule AND with no rule at all
        // (the realistic default, where the write arrives as the mode's `Ask`).
        let p = PathBuf::from("/project/.stepper/setting.json");
        for rules in [
            RuleSet::from_lists(&["Write(/**)".into()], &[], &[]),
            RuleSet::default(),
        ] {
            assert_eq!(
                evaluate(&PermissionRequest::Write(p.clone()), &rules, &root(), None, PermissionMode::Bypass),
                Decision::Deny,
                "protected .stepper write must fail closed in Bypass",
            );
            // A mode with a human still escalates to an explicit prompt.
            assert_eq!(
                evaluate(&PermissionRequest::Write(p.clone()), &rules, &root(), None, PermissionMode::AcceptEdits),
                Decision::Ask,
            );
        }
    }

    #[test]
    fn additional_directories_count_as_in_project_for_mode_defaults() {
        let extra = PathBuf::from("/extra");
        let rules = RuleSet::default().with_additional_dirs(vec![extra.clone()]);
        // In Auto an in-project write auto-allows; an `additionalDirectories` entry
        // gets the same treatment instead of escalating as out-of-project.
        assert_eq!(
            evaluate(&PermissionRequest::Write(extra.join("a.txt")), &rules, &root(), None, PermissionMode::Auto),
            Decision::Allow,
        );
        // A path in neither the project nor an additional dir still prompts.
        assert_eq!(
            evaluate(
                &PermissionRequest::Write("/elsewhere/a.txt".into()),
                &rules,
                &root(),
                None,
                PermissionMode::Auto,
            ),
            Decision::Ask,
        );
    }

    #[test]
    fn auto_mode_auto_allows_plain_shell_but_still_gates_redirects_and_denies() {
        // Auto with NO allow rules now auto-allows an ordinary command (the user's
        // "stop nagging me" intent) …
        let rules = RuleSet::default();
        assert_eq!(
            evaluate(
                &PermissionRequest::Bash("cargo build".into()),
                &rules,
                &root(),
                None,
                PermissionMode::Auto,
            ),
            Decision::Allow
        );
        // … but an out-of-project redirect target still asks (gated as a Write) …
        assert_eq!(
            evaluate(
                &PermissionRequest::Bash("echo x > /etc/passwd".into()),
                &rules,
                &root(),
                None,
                PermissionMode::Auto,
            ),
            Decision::Ask
        );
        // … a dynamic (unanalyzable) redirect target still escalates to Ask …
        assert_eq!(
            evaluate(
                &PermissionRequest::Bash("echo x > $FILE".into()),
                &rules,
                &root(),
                None,
                PermissionMode::Auto,
            ),
            Decision::Ask
        );
        // … and an explicit deny still wins — the guardrail that catches a
        // destructive verb (incl. one hidden in a substitution like
        // `echo $(rm -rf /)`, whose inner atom auto-allows in Auto without it).
        let denied = RuleSet::from_lists(&[], &[], &["Bash(rm -rf *)".into()]);
        assert_eq!(
            evaluate(
                &PermissionRequest::Bash("rm -rf /".into()),
                &denied,
                &root(),
                None,
                PermissionMode::Auto,
            ),
            Decision::Deny
        );
    }

    #[test]
    fn non_auto_modes_still_ask_for_unruled_shell() {
        let rules = RuleSet::default();
        for mode in [PermissionMode::Default, PermissionMode::AcceptEdits, PermissionMode::Plan] {
            assert_eq!(
                evaluate(
                    &PermissionRequest::Bash("cargo build".into()),
                    &rules,
                    &root(),
                    None,
                    mode,
                ),
                Decision::Ask,
                "mode {mode:?} must keep shell gated"
            );
        }
        // headless `dont-ask` denies it (fail closed).
        assert_eq!(
            evaluate(
                &PermissionRequest::Bash("cargo build".into()),
                &rules,
                &root(),
                None,
                PermissionMode::DontAsk,
            ),
            Decision::Deny
        );
    }

    #[test]
    fn auto_mode_auto_allows_webfetch_and_mcp() {
        let rules = RuleSet::default();
        assert_eq!(
            evaluate(
                &PermissionRequest::WebFetch("https://example.com".into()),
                &rules,
                &root(),
                None,
                PermissionMode::Auto,
            ),
            Decision::Allow
        );
        assert_eq!(
            evaluate(
                &PermissionRequest::Mcp { server: "fs".into(), tool: "read".into() },
                &rules,
                &root(),
                None,
                PermissionMode::Auto,
            ),
            Decision::Allow
        );
        // default mode still asks
        assert_eq!(
            evaluate(
                &PermissionRequest::WebFetch("https://example.com".into()),
                &rules,
                &root(),
                None,
                PermissionMode::Default,
            ),
            Decision::Ask
        );
    }

    #[test]
    fn explicit_deny_overrides_accept_edits() {
        let rules = RuleSet::from_lists(&[], &[], &["Write(//etc/**)".into()]);
        assert_eq!(
            evaluate(
                &PermissionRequest::Write("/etc/passwd".into()),
                &rules,
                &root(),
                None,
                PermissionMode::AcceptEdits,
            ),
            Decision::Deny
        );
    }

    #[test]
    fn auto_mode_auto_allows_read_only_outside_project() {
        let rules = RuleSet::default();
        // Policy A: a read outside the project is auto-approved in Auto — a read
        // can't damage the tree and secret paths are still screened at the tool
        // layer. (Previously Auto gated reads by project, more restrictive than
        // even Default mode, which already allows reads anywhere.)
        assert_eq!(
            evaluate(&PermissionRequest::Read("/elsewhere/notes.md".into()), &rules, &root(), None, PermissionMode::Auto),
            Decision::Allow,
        );
        // … but a WRITE outside the project still asks (only read-only tools are
        // ungated; mutating tools stay project-bounded).
        assert_eq!(
            evaluate(&PermissionRequest::Write("/elsewhere/notes.md".into()), &rules, &root(), None, PermissionMode::Auto),
            Decision::Ask,
        );
        // … and an in-project write is still auto-allowed in Auto.
        assert_eq!(
            evaluate(&PermissionRequest::Write(root().join("a.txt")), &rules, &root(), None, PermissionMode::Auto),
            Decision::Allow,
        );
    }
}
