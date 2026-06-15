//! Slash-command expansion: find `.stepper/commands/<name>.md`, run the
//! substitution engine (shell/args/file/env/include), and hand the expanded text
//! to the orchestrator as the turn prompt.
//!
//! Security: the side-effecting substitutions (`!`shell``, `{file:…}`, `@include`,
//! `{env:…}`) are gated by the permission engine — the same `deny > ask > allow`
//! policy the tools use. Because expansion happens before a turn (off the
//! interactive approval channel), it is **fail-closed**: only an explicit `Allow`
//! runs; `Ask`/`Deny` are refused with a message. So a model-planted command file
//! cannot smuggle un-vetted shell/file reads into the prompt.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use stepper_config::{parse_command, substitute, CommandArgs, SubstitutionIo};
use stepper_permission::{evaluate, path, Decision, PermissionMode, PermissionRequest, RuleSet};
use stepper_tools::secret::is_secret_path;

/// Expand `/name args` into a prompt, or `None` if the command isn't found.
/// Runs synchronously (it may execute `!`shell`` substitutions), so call it from
/// a blocking context. Side effects are gated by `rules`/`mode` (fail-closed).
pub fn expand(
    project_root: PathBuf,
    home: Option<PathBuf>,
    cwd: PathBuf,
    rules: Arc<RuleSet>,
    mode: PermissionMode,
    name: String,
    args: String,
) -> Option<String> {
    let def = find_command(&project_root, home.as_deref(), &name)?;
    // Quote-aware split so `/cmd "arg with spaces"` is one argument.
    let positional: Vec<String> = shell_words::split(&args)
        .unwrap_or_else(|_| args.split_whitespace().map(str::to_string).collect());

    let mut named = BTreeMap::new();
    for (i, arg_name) in def.arguments.iter().enumerate() {
        if let Some(value) = positional.get(i) {
            named.insert(arg_name.clone(), value.clone());
        }
    }

    let io = CoreIo {
        cwd,
        project_root,
        home,
        rules,
        mode,
    };
    substitute(
        &def.template,
        &CommandArgs { positional, named },
        &io,
    )
    .ok()
}

fn find_command(
    project_root: &std::path::Path,
    home: Option<&std::path::Path>,
    name: &str,
) -> Option<stepper_config::CommandDef> {
    let bases = [
        Some(project_root.join(".stepper")),
        home.map(|h| h.join(".stepper")),
    ];
    for base in bases.into_iter().flatten() {
        let path = base.join("commands").join(format!("{name}.md"));
        if let Ok(content) = std::fs::read_to_string(&path)
            && let Ok(def) = parse_command(name, &content)
        {
            return Some(def);
        }
    }
    None
}

struct CoreIo {
    cwd: PathBuf,
    project_root: PathBuf,
    home: Option<PathBuf>,
    rules: Arc<RuleSet>,
    mode: PermissionMode,
}

impl CoreIo {
    fn resolve(&self, path: &str) -> PathBuf {
        let p = std::path::Path::new(path);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.cwd.join(p)
        }
    }

    fn allowed(&self, request: &PermissionRequest) -> bool {
        evaluate(
            request,
            &self.rules,
            &self.project_root,
            self.home.as_deref(),
            self.mode,
        ) == Decision::Allow
    }
}

impl CoreIo {
    /// The stored-RCE gate. A `.stepper/commands/*.md` file can be model-planted,
    /// so shell substitution is **rule-only**: an explicit `allow` rule must match.
    /// The active mode is deliberately NOT consulted — Auto/Bypass auto-allow
    /// ordinary shell for the interactive agent, but must never silently run a
    /// command file's `!`shell``. Evaluating under `Default` (which never
    /// auto-allows bash by mode) reduces this to "an allow rule, or refuse".
    fn shell_allowed_rule_only(&self, cmd: &str) -> bool {
        evaluate(
            &PermissionRequest::Bash(cmd.to_string()),
            &self.rules,
            &self.project_root,
            self.home.as_deref(),
            PermissionMode::Default,
        ) == Decision::Allow
    }
}

impl SubstitutionIo for CoreIo {
    fn run_shell(&self, cmd: &str) -> Result<String, String> {
        if !self.shell_allowed_rule_only(cmd) {
            return Err(format!(
                "shell `{cmd}` is not permitted in a slash command — add an explicit `allow` rule (e.g. Bash({cmd})) to run it"
            ));
        }
        // `bash -c` (not `-lc`): don't source the user's login profile into a
        // model-/command-driven shell (aliases/functions would change behavior).
        let output = std::process::Command::new("bash")
            .arg("-c")
            .arg(cmd)
            .current_dir(&self.cwd)
            .output()
            .map_err(|e| e.to_string())?;
        Ok(String::from_utf8_lossy(&output.stdout).trim_end().to_string())
    }

    fn read_file(&self, p: &str) -> Result<String, String> {
        self.read_guarded(p, "reading")
    }

    fn read_env(&self, var: &str) -> Option<String> {
        // Never leak secret-bearing env vars into a prompt via `{env:…}`.
        if is_secret_env(var) {
            return None;
        }
        let value = std::env::var(var).ok()?;
        // Refuse connection strings that embed credentials (scheme://user:pass@host).
        if looks_like_credential_value(&value) {
            return None;
        }
        Some(value)
    }

    fn include(&self, p: &str) -> Result<String, String> {
        self.read_guarded(p, "including")
    }
}

impl CoreIo {
    /// A read used by `{file:…}` / `@include` / bare `@path`. This runs during
    /// command expansion, off the interactive approval channel, so it is strict:
    /// the secret-file denylist always wins, reads are confined to the project
    /// (Plan mode's blanket read-allow is not trusted here), and an explicit
    /// `Allow` decision is still required.
    fn read_guarded(&self, p: &str, verb: &str) -> Result<String, String> {
        let resolved = self.resolve(p);
        if is_secret_path(&resolved) {
            return Err(format!("{verb} `{p}` is blocked: it looks like a secret file"));
        }
        if !path::is_in_project(&resolved, &self.project_root) {
            return Err(format!(
                "{verb} `{p}` is not permitted in a slash command (outside the project)"
            ));
        }
        if !self.allowed(&PermissionRequest::Read(resolved.clone())) {
            return Err(format!("{verb} `{p}` is not permitted in a slash command"));
        }
        std::fs::read_to_string(resolved).map_err(|e| e.to_string())
    }
}

/// Heuristic denylist for credential-bearing env var names (case-insensitive), so
/// `{env:AWS_SECRET_ACCESS_KEY}` / `{env:STEPPER_*_API_KEY}` can't be exfiltrated
/// into a prompt.
fn is_secret_env(var: &str) -> bool {
    let v = var.to_ascii_uppercase();
    v.ends_with("_KEY")
        || v.ends_with("_TOKEN")
        || v.ends_with("_SECRET")
        || v.ends_with("_URL")
        || v.ends_with("_URI")
        || v.ends_with("_PWD")
        || v.contains("SECRET")
        || v.contains("PASSWORD")
        || v.contains("PASSWD")
        || v.contains("CREDENTIAL")
        || v.contains("API_KEY")
        || v.contains("ACCESS_TOKEN")
        || v.contains("COOKIE")
        || v.contains("SESSION")
        || v.contains("AUTH")
        || v.starts_with("AWS_")
        || v.starts_with("GITHUB_TOKEN")
}

/// A value that embeds credentials, e.g. a `scheme://user:pass@host` connection
/// string — refused even when the var name looks innocuous (`DATABASE_URL`).
fn looks_like_credential_value(value: &str) -> bool {
    if let Some((_, after_scheme)) = value.split_once("://")
        && let Some(authority) = after_scheme.split('/').next()
    {
        return authority.contains('@') && authority.contains(':');
    }
    false
}
