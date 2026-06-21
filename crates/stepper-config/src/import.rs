//! `stepper import` — detect another agent's *global* config and migrate the
//! portable parts into the global `~/.stepper/` contract. Two sources are
//! understood today: Claude Code (`~/.claude/` + `~/.claude.json`) and Codex
//! (`~/.codex/`).
//!
//! The migration is non-destructive and idempotent:
//! - instruction docs (`CLAUDE.md`, `AGENTS.md`) append to `stepper.md` under a
//!   stable source marker, skipped if that marker line is already present;
//! - `setting.json` `permissions` and `mcpServers` *union* in (existing entries
//!   are never overwritten), with Claude `mcp__server[__tool]` permission
//!   specifiers rewritten to stepper's `Mcp(server[, tool])`;
//! - skill/command files copy only when the target path does not exist.
//!
//! The destination `~/.stepper/setting.json` is treated as precious: if it
//! exists but does not parse (or is not a JSON object), the import aborts rather
//! than clobbering it. Read-only *source* files are lenient — a malformed one is
//! noted and skipped, never fatal.
//!
//! [`build_plan`] computes the whole plan without touching disk (for a preview);
//! [`apply_plan`] commits it, re-reading the destination at write time so a
//! concurrent edit is preserved. Re-running yields an empty plan.

use crate::imports::relocate_imports;
use crate::scaffold;
use serde::Deserialize;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

/// Which agent(s) to import from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportFrom {
    Claude,
    Codex,
    Cursor,
    Gemini,
    All,
}

impl ImportFrom {
    /// Parse the CLI/slash argument (`claude` | `codex` | `cursor` | `gemini` |
    /// `all`; empty = `all`).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            "cursor" => Some(Self::Cursor),
            "gemini" => Some(Self::Gemini),
            "all" | "" => Some(Self::All),
            _ => None,
        }
    }
    fn wants_claude(self) -> bool {
        matches!(self, Self::Claude | Self::All)
    }
    fn wants_codex(self) -> bool {
        matches!(self, Self::Codex | Self::All)
    }
    fn wants_cursor(self) -> bool {
        matches!(self, Self::Cursor | Self::All)
    }
    fn wants_gemini(self) -> bool {
        matches!(self, Self::Gemini | Self::All)
    }
}

/// A source artifact found on disk (shown in the preview's "detected" list).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedSource {
    pub agent: &'static str,
    pub description: String,
    pub path: PathBuf,
}

/// An instruction block to append to `stepper.md`, tagged with a stable marker
/// so a re-import skips it.
#[derive(Debug, Clone)]
pub struct InstructionSection {
    pub marker: String,
    pub title: String,
    pub body: String,
}

/// A file (or skill directory) to copy when the destination is absent.
#[derive(Debug, Clone)]
pub struct FileCopy {
    pub from: PathBuf,
    pub to: PathBuf,
    pub label: String,
}

/// The computed migration plan: a faithful preview that is also exactly what
/// [`apply_plan`] writes.
#[derive(Debug, Clone)]
pub struct ImportPlan {
    pub home: PathBuf,
    pub target_dir: PathBuf,
    pub sources: Vec<DetectedSource>,
    pub sections: Vec<InstructionSection>,
    pub stepper_md_path: PathBuf,
    pub settings_path: PathBuf,
    /// The merged settings, for preview/inspection. `apply_plan` re-derives the
    /// write from a fresh read of the destination (TOCTOU-safe).
    pub settings_after: Value,
    pub settings_changed: bool,
    pub permission_adds: Vec<(String, String)>,
    /// Newly-added MCP servers (name → config), re-applied at write time.
    pub mcp_additions: Vec<(String, Value)>,
    pub mcp_adds: Vec<String>,
    /// Top-level `defaultModel` derived from an imported config (Codex
    /// `model`/`model_provider`, Claude `model`); first-writer-wins. Applied
    /// only when the destination has none (never clobbers a user's).
    pub default_model: Option<String>,
    /// Synthesized `providers.<name>` entries (name → ProviderConfig value) for
    /// the imported model's provider, unioned keep-existing at write time.
    pub provider_adds: Vec<(String, Value)>,
    pub file_copies: Vec<FileCopy>,
    pub notes: Vec<String>,
}

impl ImportPlan {
    /// Nothing left to migrate (everything already present).
    pub fn is_empty(&self) -> bool {
        self.sections.is_empty() && !self.settings_changed && self.file_copies.is_empty()
    }
}

/// What `apply_plan` actually wrote.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportSummary {
    pub sections_appended: usize,
    pub settings_written: bool,
    pub files_copied: usize,
}

/// Compute the migration plan for `home` without writing anything. Errors if the
/// destination `~/.stepper/setting.json` exists but is not a parseable JSON
/// object (so the import never clobbers a file it cannot understand).
pub fn build_plan(home: &Path, from: ImportFrom) -> io::Result<ImportPlan> {
    let target_dir = home.join(".stepper");
    let settings_path = target_dir.join("setting.json");
    let stepper_md_path = target_dir.join("stepper.md");

    let mut sources = Vec::new();
    let mut notes = Vec::new();

    let mut settings = read_json(&settings_path)?.unwrap_or_else(default_settings);
    validate_destination(&settings).map_err(|why| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{} {why} — fix or remove it before importing",
                tilde(&settings_path, home)
            ),
        )
    })?;

    let mut permission_adds = Vec::new();
    let mut mcp_adds = Vec::new();
    let mut mcp_additions = Vec::new();
    let mut file_copies = Vec::new();

    let existing_md = read_text(&stepper_md_path)?.unwrap_or_default();
    let mut sections = Vec::new();
    let mut model_import = ModelImport::default();

    if from.wants_claude() {
        collect_claude(
            home,
            &existing_md,
            &mut sources,
            &mut sections,
            &mut settings,
            &mut permission_adds,
            &mut mcp_adds,
            &mut mcp_additions,
            &mut file_copies,
            &mut model_import,
            &mut notes,
        )?;
    }
    if from.wants_codex() {
        collect_codex(
            home,
            &existing_md,
            &mut sources,
            &mut sections,
            &mut settings,
            &mut mcp_adds,
            &mut mcp_additions,
            &mut model_import,
            &mut notes,
        )?;
    }
    if from.wants_cursor() {
        collect_cursor(home, &existing_md, &mut sources, &mut sections)?;
    }
    if from.wants_gemini() {
        collect_gemini(home, &existing_md, &mut sources, &mut sections)?;
    }

    let settings_changed = !permission_adds.is_empty()
        || !mcp_adds.is_empty()
        || model_import.default_model.is_some()
        || !model_import.providers.is_empty();

    Ok(ImportPlan {
        home: home.to_path_buf(),
        target_dir,
        sources,
        sections,
        stepper_md_path,
        settings_path,
        settings_after: settings,
        settings_changed,
        permission_adds,
        mcp_additions,
        mcp_adds,
        default_model: model_import.default_model,
        provider_adds: model_import.providers,
        file_copies,
        notes,
    })
}

/// Commit a plan: create the skeleton, append instruction sections, write the
/// merged `setting.json` (re-reading the destination so a concurrent edit is
/// preserved), and copy the queued files. Idempotent.
pub fn apply_plan(plan: &ImportPlan) -> io::Result<ImportSummary> {
    scaffold::ensure_skeleton(&plan.home)?;
    let mut summary = ImportSummary::default();

    if !plan.sections.is_empty() {
        let mut doc = read_text(&plan.stepper_md_path)?.unwrap_or_default();
        if doc.is_empty() {
            doc.push_str("# stepper base context\n\n> Imported by `stepper import`.\n");
        }
        // Guard against markers already in the *pre-existing* doc plus the ones we
        // emit this run — never against a freshly-appended section's body, so a
        // marker string carried inside one section can't shadow a later section.
        let mut seen: std::collections::HashSet<String> = marker_lines(&doc).collect();
        for section in &plan.sections {
            if !seen.insert(section.marker.clone()) {
                continue;
            }
            doc = format!(
                "{}\n\n{}\n## {}\n\n{}\n",
                doc.trim_end(),
                section.marker,
                section.title,
                section.body.trim_end()
            );
            summary.sections_appended += 1;
        }
        std::fs::write(&plan.stepper_md_path, doc)?;
    }

    if plan.settings_changed {
        // Re-read the destination instead of trusting the build-time snapshot, so
        // an edit made between preview and apply survives; only *our* additions
        // are unioned on top. A destination that became unparseable since build
        // aborts rather than clobbering.
        let mut settings = read_json(&plan.settings_path)?.unwrap_or_else(default_settings);
        validate_destination(&settings).map_err(|why| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} {why}", tilde(&plan.settings_path, &plan.home)),
            )
        })?;
        for (verdict, spec) in &plan.permission_adds {
            union_permission(&mut settings, verdict, spec);
        }
        for (name, cfg) in &plan.mcp_additions {
            union_mcp(&mut settings, name, cfg.clone());
        }
        for (name, cfg) in &plan.provider_adds {
            union_provider(&mut settings, name, cfg.clone());
        }
        if let Some(m) = &plan.default_model {
            set_default_model_if_absent(&mut settings, m);
        }
        if let Some(parent) = plan.settings_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(
            &plan.settings_path,
            format!("{}\n", serde_json::to_string_pretty(&settings)?),
        )?;
        summary.settings_written = true;
    }

    for copy in &plan.file_copies {
        // Symlink-aware: a dangling symlink at the destination still counts as
        // "present" (keep it) rather than triggering an impossible rename.
        if path_present(&copy.to) {
            continue;
        }
        copy_into_place(&copy.from, &copy.to)?;
        summary.files_copied += 1;
    }

    Ok(summary)
}

#[allow(clippy::too_many_arguments)]
fn collect_claude(
    home: &Path,
    existing_md: &str,
    sources: &mut Vec<DetectedSource>,
    sections: &mut Vec<InstructionSection>,
    settings: &mut Value,
    permission_adds: &mut Vec<(String, String)>,
    mcp_adds: &mut Vec<String>,
    mcp_additions: &mut Vec<(String, Value)>,
    file_copies: &mut Vec<FileCopy>,
    model_import: &mut ModelImport,
    notes: &mut Vec<String>,
) -> io::Result<()> {
    let claude_dir = home.join(".claude");

    // 1. Instructions: ~/.claude/CLAUDE.md (with its @imports kept resolvable).
    let claude_md = claude_dir.join("CLAUDE.md");
    if let Some(text) = read_text(&claude_md)?.filter(|t| !t.trim().is_empty()) {
        sources.push(DetectedSource {
            agent: "claude",
            description: "instructions (CLAUDE.md)".into(),
            path: claude_md.clone(),
        });
        push_section(
            sections,
            existing_md,
            "claude/CLAUDE.md",
            &format!("Imported from Claude Code ({})", tilde(&claude_md, home)),
            &relocate_imports(&text, &claude_dir, Some(home)),
        );
    }

    // 2. Permissions: ~/.claude/settings.json → permissions.{allow,ask,deny}.
    let settings_json = claude_dir.join("settings.json");
    if let Some(value) = read_source_json(&settings_json, "~/.claude/settings.json", notes)? {
        sources.push(DetectedSource {
            agent: "claude",
            description: "permissions (settings.json)".into(),
            path: settings_json,
        });
        for verdict in ["allow", "ask", "deny"] {
            for spec in str_array(value.pointer(&format!("/permissions/{verdict}"))) {
                let converted = convert_permission(&spec);
                if union_permission(settings, verdict, &converted) {
                    permission_adds.push((verdict.to_string(), converted));
                }
            }
        }
        // Claude model ids are bare (no provider prefix) → anthropic.
        if let Some(model) = value.get("model").and_then(Value::as_str).filter(|m| !m.is_empty()) {
            record_model(settings, model_import, "anthropic", model, notes);
        }
        if has_nonpermission_keys(&value) {
            notes.push(
                "Claude-only settings (statusLine, theme, effortLevel, plugins, …) are not portable — skipped".into(),
            );
        }
    }

    // 3. MCP servers: global ones live in ~/.claude.json under `mcpServers`.
    let claude_json = home.join(".claude.json");
    if let Some(value) = read_source_json(&claude_json, "~/.claude.json", notes)?
        && let Some(servers) = value.get("mcpServers").and_then(Value::as_object)
    {
        if !servers.is_empty() {
            sources.push(DetectedSource {
                agent: "claude",
                description: format!("{} MCP server(s) (~/.claude.json)", servers.len()),
                path: claude_json,
            });
        }
        for (name, cfg) in servers {
            add_mcp(settings, name, cfg.clone(), "claude", mcp_adds, mcp_additions, notes);
        }
    }

    // 4. Skills: ~/.claude/skills/<name>/SKILL.md → copy the dir when absent.
    let skills_dir = claude_dir.join("skills");
    let stepper_canonical = canonical_stepper_dir(home);
    for (name, dir) in subdirs_with(&skills_dir, "SKILL.md") {
        let dest = home.join(".stepper").join("skills").join(&name);
        sources.push(DetectedSource {
            agent: "claude",
            description: format!("skill '{name}'"),
            path: dir.clone(),
        });
        if path_present(&dest) {
            notes.push(format!("skill '{name}' already exists — kept yours"));
            continue;
        }
        // Resolve the (possibly symlinked) skill root to a real path so the copy
        // walks a stable tree, and refuse one that overlaps `~/.stepper` in either
        // direction — a source that is a parent of the destination, or *is* the
        // destination tree, would otherwise make the copy recurse into itself.
        let Ok(real) = dir.canonicalize() else {
            notes.push(format!("skill '{name}' could not be resolved — skipped"));
            continue;
        };
        if encloses(&real, &stepper_canonical) || encloses(&stepper_canonical, &real) {
            notes.push(format!("skill '{name}' overlaps ~/.stepper — skipped"));
            continue;
        }
        // The defining `SKILL.md` must live inside the skill dir: a symlink that
        // escapes the tree would be dropped by `copy_dir_recursive` (the same
        // guard that refuses a link to `~/.ssh`), leaving an unloadable skill — so
        // skip the whole skill with a note rather than copy it half-broken.
        if let Ok(target) = real.join("SKILL.md").canonicalize()
            && !target.starts_with(&real)
        {
            notes.push(format!("skill '{name}': its SKILL.md links outside the skill dir — skipped"));
            continue;
        }
        // Validate against the *real* loader (frontmatter `name`/`description`),
        // not just the directory name, so the user is warned about skills stepper
        // will refuse to load (uppercase/underscore/reserved names, etc.).
        if let Ok(content) = std::fs::read_to_string(real.join("SKILL.md"))
            && let Err(e) = crate::frontmatter::parse_skill(&content)
        {
            notes.push(format!("skill '{name}' copied but stepper may not load it: {e}"));
        }
        file_copies.push(FileCopy {
            from: real,
            to: dest,
            label: format!("skill '{name}'"),
        });
    }

    // 5. Commands: ~/.claude/commands/*.md → copy when absent.
    let commands_dir = claude_dir.join("commands");
    let mut copied_command = false;
    for (stem, path) in md_files(&commands_dir) {
        let dest = home.join(".stepper").join("commands").join(format!("{stem}.md"));
        sources.push(DetectedSource {
            agent: "claude",
            description: format!("command '/{stem}'"),
            path: path.clone(),
        });
        if path_present(&dest) {
            notes.push(format!("command '/{stem}' already exists — kept yours"));
        } else {
            file_copies.push(FileCopy {
                from: path,
                to: dest,
                label: format!("command '/{stem}'"),
            });
            copied_command = true;
        }
    }
    if copied_command {
        notes.push(
            "copied Claude commands verbatim — `$ARGUMENTS` may need stepper's `$1`/`$name` syntax".into(),
        );
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn collect_codex(
    home: &Path,
    existing_md: &str,
    sources: &mut Vec<DetectedSource>,
    sections: &mut Vec<InstructionSection>,
    settings: &mut Value,
    mcp_adds: &mut Vec<String>,
    mcp_additions: &mut Vec<(String, Value)>,
    model_import: &mut ModelImport,
    notes: &mut Vec<String>,
) -> io::Result<()> {
    let codex_dir = home.join(".codex");

    // 1. config.toml: MCP servers + model + personality(note).
    let config_toml = codex_dir.join("config.toml");
    if let Some(text) = read_text(&config_toml)? {
        match toml::from_str::<CodexConfig>(&text) {
            Ok(config) => {
                if !config.mcp_servers.is_empty() || config.model.is_some() {
                    sources.push(DetectedSource {
                        agent: "codex",
                        description: "config.toml (MCP / model)".into(),
                        path: config_toml.clone(),
                    });
                }
                for (name, server) in &config.mcp_servers {
                    let has_url = server.url.as_deref().is_some_and(|u| !u.is_empty());
                    if server.command.is_none() && !has_url {
                        notes.push(format!(
                            "Codex MCP server '{name}' has neither command nor url — skipped"
                        ));
                        continue;
                    }
                    let cfg = server.to_stepper_value(notes, name);
                    add_mcp(settings, name, cfg, "codex", mcp_adds, mcp_additions, notes);
                }
                if let Some(model) = config.model.as_deref().filter(|m| !m.is_empty()) {
                    // Codex omits model_provider for the default OpenAI path.
                    let provider = config.model_provider.as_deref().unwrap_or("openai");
                    record_model(settings, model_import, provider, model, notes);
                }
                if config.personality.is_some() {
                    notes.push("Codex `personality` is Codex-only — skipped".into());
                }
            }
            Err(e) => notes.push(format!("could not parse ~/.codex/config.toml: {e}")),
        }
    }

    // 2. Instructions: ~/.codex/AGENTS.md and ~/AGENTS.md.
    for (path, marker, label) in [
        (codex_dir.join("AGENTS.md"), "codex/AGENTS.md", "Codex"),
        (home.join("AGENTS.md"), "home/AGENTS.md", "home"),
    ] {
        if let Some(text) = read_text(&path)?.filter(|t| !t.trim().is_empty()) {
            sources.push(DetectedSource {
                agent: "codex",
                description: format!("instructions ({})", tilde(&path, home)),
                path: path.clone(),
            });
            let base_dir = path.parent().unwrap_or(home);
            push_section(
                sections,
                existing_md,
                marker,
                &format!("Imported from {label} ({})", tilde(&path, home)),
                &relocate_imports(&text, base_dir, Some(home)),
            );
        }
    }

    // 3. Memories are a sqlite store — not portable.
    if codex_dir.join("goals_1.sqlite").exists() || codex_dir.join("memory.sqlite").exists() {
        notes.push("Codex memories are stored in sqlite — not migrated".into());
    }

    Ok(())
}

/// Cursor instructions (global only): the legacy `~/.cursorrules` file plus each
/// `.md`/`.mdc` rule directly in `~/.cursor/rules/`, one section per file.
/// Instruction-only — no settings/permissions/MCP.
fn collect_cursor(
    home: &Path,
    existing_md: &str,
    sources: &mut Vec<DetectedSource>,
    sections: &mut Vec<InstructionSection>,
) -> io::Result<()> {
    let cursorrules = home.join(".cursorrules");
    if let Some(text) = read_text(&cursorrules)?.filter(|t| !t.trim().is_empty()) {
        sources.push(DetectedSource {
            agent: "cursor",
            description: "instructions (.cursorrules)".into(),
            path: cursorrules.clone(),
        });
        push_section(
            sections,
            existing_md,
            "cursor/.cursorrules",
            &format!("Imported from Cursor ({})", tilde(&cursorrules, home)),
            &relocate_imports(&text, home, Some(home)),
        );
    }

    let rules_dir = home.join(".cursor").join("rules");
    for (stem, path) in md_or_mdc_files(&rules_dir) {
        if let Some(text) = read_text(&path)?.filter(|t| !t.trim().is_empty()) {
            sources.push(DetectedSource {
                agent: "cursor",
                description: format!("rule ({})", tilde(&path, home)),
                path: path.clone(),
            });
            push_section(
                sections,
                existing_md,
                &format!("cursor/rules/{stem}"),
                &format!("Imported from Cursor ({})", tilde(&path, home)),
                &relocate_imports(&text, &rules_dir, Some(home)),
            );
        }
    }
    Ok(())
}

/// Gemini instructions (global only): `~/.gemini/GEMINI.md`. Project-scoped
/// `./GEMINI.md` is out of scope — import operates on `~/.stepper`, never cwd.
fn collect_gemini(
    home: &Path,
    existing_md: &str,
    sources: &mut Vec<DetectedSource>,
    sections: &mut Vec<InstructionSection>,
) -> io::Result<()> {
    let gemini_dir = home.join(".gemini");
    let gemini_md = gemini_dir.join("GEMINI.md");
    if let Some(text) = read_text(&gemini_md)?.filter(|t| !t.trim().is_empty()) {
        sources.push(DetectedSource {
            agent: "gemini",
            description: "instructions (GEMINI.md)".into(),
            path: gemini_md.clone(),
        });
        push_section(
            sections,
            existing_md,
            "gemini/GEMINI.md",
            &format!("Imported from Gemini ({})", tilde(&gemini_md, home)),
            &relocate_imports(&text, &gemini_dir, Some(home)),
        );
    }
    Ok(())
}

/// Codex `config.toml` (only the fields we migrate).
#[derive(Debug, Deserialize)]
struct CodexConfig {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    model_provider: Option<String>,
    #[serde(default)]
    personality: Option<String>,
    #[serde(default)]
    mcp_servers: BTreeMap<String, CodexMcpServer>,
}

#[derive(Debug, Deserialize)]
struct CodexMcpServer {
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default, alias = "http_headers")]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    bearer_token_env_var: Option<String>,
    /// Header-name → env-var-name (Codex injects the env var's value). stepper
    /// has no env-templated headers, so these are noted, not migrated.
    #[serde(default)]
    env_http_headers: BTreeMap<String, String>,
}

impl CodexMcpServer {
    /// Convert into a stepper `mcpServers` entry (`type` + transport fields).
    fn to_stepper_value(&self, notes: &mut Vec<String>, name: &str) -> Value {
        let mut obj = Map::new();
        if let Some(url) = self.url.as_deref().filter(|u| !u.is_empty()) {
            obj.insert("type".into(), Value::String("http".into()));
            obj.insert("url".into(), Value::String(url.to_string()));
            if !self.headers.is_empty() {
                obj.insert("headers".into(), to_str_map(&self.headers));
            }
            if self.bearer_token_env_var.is_some() || !self.env_http_headers.is_empty() {
                notes.push(format!(
                    "MCP server '{name}' uses env-var-based auth headers — add them to setting.json manually"
                ));
            }
        } else {
            obj.insert("type".into(), Value::String("stdio".into()));
            if let Some(command) = self.command.as_deref() {
                obj.insert("command".into(), Value::String(command.to_string()));
            }
            if !self.args.is_empty() {
                obj.insert(
                    "args".into(),
                    Value::Array(self.args.iter().cloned().map(Value::String).collect()),
                );
            }
            if !self.env.is_empty() {
                obj.insert("env".into(), to_str_map(&self.env));
            }
        }
        Value::Object(obj)
    }
}

// ---- pure transforms ----

/// Rewrite a Claude permission specifier to stepper's grammar: `mcp__server` →
/// `Mcp(server)`, `mcp__server__tool` → `Mcp(server, tool)`, `mcp__server__*` →
/// `Mcp(server)`. Everything else (`Bash(…)`, `Read(…)`, bare `Read`, …) passes
/// through unchanged. A degenerate specifier with no server segment (`mcp__`,
/// `mcp____tool`) is left verbatim rather than turned into a rule that parses but
/// can never match.
pub fn convert_permission(spec: &str) -> String {
    let trimmed = spec.trim();
    let Some(rest) = trimmed.strip_prefix("mcp__") else {
        return trimmed.to_string();
    };
    let (server, tool) = match rest.split_once("__") {
        Some((server, tool)) => (server.trim_end_matches('_'), tool.trim()),
        None => (rest.trim_end_matches('_'), ""),
    };
    if server.is_empty() {
        return trimmed.to_string();
    }
    if tool.is_empty() || tool == "*" {
        format!("Mcp({server})")
    } else {
        format!("Mcp({server}, {tool})")
    }
}

/// Insert `spec` into `settings.permissions.<verdict>` if absent. Returns whether
/// it was added. Total: a settings/permissions value of the wrong JSON shape is a
/// no-op (never panics) — `validate_destination` rejects such files up front.
fn union_permission(settings: &mut Value, verdict: &str, spec: &str) -> bool {
    let Some(obj) = settings.as_object_mut() else {
        return false;
    };
    let perms = obj
        .entry("permissions")
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(perms) = perms.as_object_mut() else {
        return false;
    };
    let list = perms
        .entry(verdict)
        .or_insert_with(|| Value::Array(Vec::new()));
    let Some(arr) = list.as_array_mut() else {
        return false;
    };
    if arr.iter().any(|v| v.as_str() == Some(spec)) {
        return false;
    }
    arr.push(Value::String(spec.to_string()));
    true
}

/// Accumulates the model/provider derived from imported configs across sources.
/// `default_model` is first-writer-wins (Claude is collected before Codex);
/// `providers` unions keep-existing per name.
#[derive(Default)]
struct ModelImport {
    default_model: Option<String>,
    providers: Vec<(String, Value)>,
}

/// A `providers.<name>` value mirroring the CLI's `convention_provider`
/// (kind/baseUrl/auth only — the key resolves from `STEPPER_<NAME>_API_KEY` at
/// load, so no apiKey is written). The bool is whether the name is off-convention
/// (an `openai-compat`/api.openai.com fallback the caller flags for manual edit).
fn provider_entry(name: &str) -> (Value, bool) {
    let mut obj = Map::new();
    let mut ambiguous = false;
    let kind = match name {
        "anthropic" => "anthropic",
        "codex" => {
            obj.insert("auth".into(), Value::String("codex-oauth".into()));
            "openai-responses"
        }
        "openai" => {
            obj.insert("baseUrl".into(), Value::String("https://api.openai.com/v1".into()));
            "openai-compat"
        }
        "ollama-cloud" => {
            obj.insert("baseUrl".into(), Value::String("https://ollama.com/v1".into()));
            "openai-compat"
        }
        "omlx" | "mlx" => {
            obj.insert("baseUrl".into(), Value::String("http://localhost:8000/v1".into()));
            "openai-compat"
        }
        _ => {
            obj.insert("baseUrl".into(), Value::String("https://api.openai.com/v1".into()));
            ambiguous = true;
            "openai-compat"
        }
    };
    obj.insert("kind".into(), Value::String(kind.into()));
    (Value::Object(obj), ambiguous)
}

/// Record an imported `<provider>/<model>`: synthesize its provider entry and
/// set `defaultModel`, but only for what the destination `settings` snapshot
/// doesn't already have — mirroring the mcp/permission union so a re-import is a
/// no-op. `defaultModel` is first-writer-wins (Claude before Codex).
fn record_model(
    settings: &mut Value,
    mi: &mut ModelImport,
    provider: &str,
    model: &str,
    notes: &mut Vec<String>,
) {
    if !mi.providers.iter().any(|(n, _)| n == provider) {
        let (entry, ambiguous) = provider_entry(provider);
        if union_provider(settings, provider, entry.clone()) {
            if ambiguous {
                notes.push(format!(
                    "imported model provider '{provider}' is not a known convention — set its base URL / API key in setting.json"
                ));
            }
            mi.providers.push((provider.to_string(), entry));
        }
    }
    let model_ref = format!("{provider}/{model}");
    if mi.default_model.is_none() {
        if set_default_model_if_absent(settings, &model_ref) {
            mi.default_model = Some(model_ref);
        } else {
            notes.push(format!(
                "kept your existing defaultModel — imported '{model_ref}' not applied"
            ));
        }
    } else if mi.default_model.as_deref() != Some(model_ref.as_str()) {
        notes.push(format!(
            "kept the first imported defaultModel '{}' — also saw '{model_ref}'",
            mi.default_model.as_deref().unwrap_or_default()
        ));
    }
}

/// Insert a provider under `settings.providers.<name>` if that name is absent.
/// Returns whether it was added (keep-existing, total on wrong shapes).
fn union_provider(settings: &mut Value, name: &str, cfg: Value) -> bool {
    let Some(obj) = settings.as_object_mut() else {
        return false;
    };
    let providers = obj
        .entry("providers")
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(providers) = providers.as_object_mut() else {
        return false;
    };
    if providers.contains_key(name) {
        return false;
    }
    providers.insert(name.to_string(), cfg);
    true
}

/// Set top-level `defaultModel` only when the destination has none (never
/// clobber a user's). Returns whether it was set. Total on a non-object.
fn set_default_model_if_absent(settings: &mut Value, model: &str) -> bool {
    let Some(obj) = settings.as_object_mut() else {
        return false;
    };
    if obj.contains_key("defaultModel") {
        return false;
    }
    obj.insert("defaultModel".into(), Value::String(model.to_string()));
    true
}

/// Insert an MCP server under `settings.mcpServers.<name>` if that name is
/// absent. Returns whether it was added.
fn union_mcp(settings: &mut Value, name: &str, cfg: Value) -> bool {
    let Some(obj) = settings.as_object_mut() else {
        return false;
    };
    let servers = obj
        .entry("mcpServers")
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(servers) = servers.as_object_mut() else {
        return false;
    };
    if servers.contains_key(name) {
        return false;
    }
    servers.insert(name.to_string(), cfg);
    true
}

/// Union one MCP server and record it for preview + write-time re-apply, noting
/// a collision (distinguishing a pre-existing entry from a same-run cross-source
/// duplicate).
fn add_mcp(
    settings: &mut Value,
    name: &str,
    cfg: Value,
    agent: &str,
    mcp_adds: &mut Vec<String>,
    mcp_additions: &mut Vec<(String, Value)>,
    notes: &mut Vec<String>,
) {
    let added_this_run = mcp_adds.iter().any(|m| m.starts_with(&format!("{name} (")));
    if union_mcp(settings, name, cfg.clone()) {
        mcp_adds.push(format!("{name} ({agent})"));
        mcp_additions.push((name.to_string(), cfg));
    } else if added_this_run {
        notes.push(format!(
            "MCP server '{name}' is in more than one source — kept the first"
        ));
    } else {
        notes.push(format!(
            "MCP server '{name}' already in your setting.json — kept yours"
        ));
    }
}

/// Whether a Claude `settings.json` carries keys beyond `permissions` and the
/// `model` we migrate (so the preview can note the rest are dropped).
fn has_nonpermission_keys(value: &Value) -> bool {
    value
        .as_object()
        .map(|o| o.keys().any(|k| k != "permissions" && k != "model"))
        .unwrap_or(false)
}

/// Reject a destination `setting.json` whose shape would make a non-destructive
/// merge impossible: it must be a JSON object, `permissions` (if present) an
/// object with array `allow`/`ask`/`deny`, and `mcpServers` (if present) an
/// object. `Ok(())` for the common cases (absent file → `default_settings`, or a
/// well-formed config).
fn validate_destination(settings: &Value) -> Result<(), String> {
    let Some(obj) = settings.as_object() else {
        return Err("is not a JSON object".into());
    };
    if let Some(perms) = obj.get("permissions") {
        let Some(perms) = perms.as_object() else {
            return Err("has a non-object `permissions`".into());
        };
        for verdict in ["allow", "ask", "deny"] {
            if let Some(list) = perms.get(verdict)
                && !list.is_array()
            {
                return Err(format!("has a non-array `permissions.{verdict}`"));
            }
        }
    }
    if let Some(servers) = obj.get("mcpServers")
        && !servers.is_object()
    {
        return Err("has a non-object `mcpServers`".into());
    }
    if let Some(providers) = obj.get("providers")
        && !providers.is_object()
    {
        return Err("has a non-object `providers`".into());
    }
    Ok(())
}

/// Queue an instruction section unless its marker line is already in
/// `existing_md` (anchored to a whole line so a marker string embedded in body
/// text doesn't shadow a real section).
fn push_section(
    sections: &mut Vec<InstructionSection>,
    existing_md: &str,
    id: &str,
    title: &str,
    body: &str,
) {
    let marker = format!("<!-- stepper-import:{id} -->");
    if has_marker_line(existing_md, &marker) || body.trim().is_empty() {
        return;
    }
    sections.push(InstructionSection {
        marker,
        title: title.to_string(),
        body: body.to_string(),
    });
}

/// Whether `marker` appears as its own (trimmed) line in `text`.
fn has_marker_line(text: &str, marker: &str) -> bool {
    text.lines().any(|line| line.trim() == marker)
}

/// Every stepper-import marker that appears as its own line in `text`.
fn marker_lines(text: &str) -> impl Iterator<Item = String> + '_ {
    text.lines().filter_map(|line| {
        let t = line.trim();
        t.starts_with("<!-- stepper-import:").then(|| t.to_string())
    })
}

/// Symlink-aware existence: true even for a dangling symlink (so a broken link at
/// the destination is treated as "present, keep it" rather than overwritten).
fn path_present(path: &Path) -> bool {
    path.symlink_metadata().is_ok()
}

/// The canonical path of `~/.stepper`. Resolves the real dir when it exists
/// (handling a symlinked `.stepper`); otherwise canonical home + `.stepper`.
/// Used to refuse a skill symlink that overlaps the destination tree.
fn canonical_stepper_dir(home: &Path) -> PathBuf {
    let stepper = home.join(".stepper");
    stepper.canonicalize().unwrap_or_else(|_| {
        home.canonicalize()
            .unwrap_or_else(|_| home.to_path_buf())
            .join(".stepper")
    })
}

/// Whether `outer` is an ancestor of (or equal to) `inner` — i.e. copying from
/// `outer` would descend into `inner`.
fn encloses(outer: &Path, inner: &Path) -> bool {
    inner.starts_with(outer)
}

// ---- io helpers ----

fn default_settings() -> Value {
    serde_json::json!({ "$schema": "stepper://setting.schema.json" })
}

fn read_text(path: &Path) -> io::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// JSONC read for the *destination*: `None` if absent, but a present file that
/// does not parse is an `InvalidData` error (never silently dropped — that would
/// let the import clobber a file it could not read). Uses the same JSONC parser
/// as `Config::load` so an annotated (commented) `setting.json` is accepted, not
/// rejected as the import's "unreadable destination" safety net.
fn read_json(path: &Path) -> io::Result<Option<Value>> {
    match read_text(path)? {
        Some(raw) => crate::parse_setting_jsonc(&raw).map(Some).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} is not valid JSON: {e}", path.display()),
            )
        }),
        None => Ok(None),
    }
}

/// Lenient JSON read for a read-only *source*: `None` if absent, and a parse
/// error becomes a note + `None` (a malformed Claude file is skipped, not fatal).
fn read_source_json(path: &Path, label: &str, notes: &mut Vec<String>) -> io::Result<Option<Value>> {
    let Some(raw) = read_text(path)? else {
        return Ok(None);
    };
    match serde_json::from_str(&raw) {
        Ok(value) => Ok(Some(value)),
        Err(e) => {
            notes.push(format!("could not parse {label}: {e} — skipped"));
            Ok(None)
        }
    }
}

/// The string elements of a JSON array at `value` (empty for non-arrays).
fn str_array(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default()
}

fn to_str_map(map: &BTreeMap<String, String>) -> Value {
    Value::Object(
        map.iter()
            .map(|(k, v)| (k.clone(), Value::String(v.clone())))
            .collect(),
    )
}

/// `(name, dir)` for each immediate subdirectory of `parent` that contains
/// `required`, sorted by name. A symlinked skill directory is followed (skills
/// are commonly symlinked to a shared store, e.g. `find-skills`); cycle safety
/// comes from `copy_dir_recursive`, which skips symlinks *inside* the tree.
/// Dotfile names are ignored — skills never start with `.`, and skipping them
/// keeps a `.{name}.import-tmp` staging leftover from being seen as a skill.
fn subdirs_with(parent: &Path, required: &str) -> Vec<(String, PathBuf)> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(parent) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir()
            && path.join(required).is_file()
            && let Some(name) = path.file_name().and_then(|n| n.to_str())
            && !name.starts_with('.')
        {
            found.push((name.to_string(), path));
        }
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found
}

/// `(stem, path)` for each `*.md` directly in `dir`, sorted by stem.
fn md_files(dir: &Path) -> Vec<(String, PathBuf)> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|x| x.to_str()) == Some("md")
            && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
        {
            found.push((stem.to_string(), path));
        }
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found
}

/// `(stem, path)` for each `*.md`/`*.mdc` directly in `dir`, sorted by stem.
/// Sibling of `md_files` (Cursor rules use `.mdc`; the commands copy stays
/// `.md`-only).
fn md_or_mdc_files(dir: &Path) -> Vec<(String, PathBuf)> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if matches!(path.extension().and_then(|x| x.to_str()), Some("md" | "mdc"))
            && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
        {
            found.push((stem.to_string(), path));
        }
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found
}

/// Copy `from` to `to` atomically: a directory (`from` is a resolved real path)
/// is staged into a sibling temp dir and renamed into place (so a failed copy
/// leaves no half-written destination, and the staging dir is removed on *any*
/// error); a file is copied directly. The caller guarantees `to` does not exist.
fn copy_into_place(from: &Path, to: &Path) -> io::Result<()> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if from.is_dir() {
        let staging = staging_path(to);
        // Structural self-copy backstop, canonical and at copy time (so it can't be
        // defeated by a symlinked `.stepper`, a non-canonicalizable home, or a
        // build→apply swap): refuse when `from` and the staging dir enclose each
        // other, which would make the walk recurse into what it is writing.
        if overlaps(from, &staging) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("refusing to copy {}: it overlaps the import staging dir", from.display()),
            ));
        }
        // Clear any stale staging slot, whether a leftover dir or a stray file.
        let _ = std::fs::remove_dir_all(&staging);
        let _ = std::fs::remove_file(&staging);
        let staged = copy_dir_recursive(from, from, &staging).and_then(|()| std::fs::rename(&staging, to));
        if staged.is_err() {
            let _ = std::fs::remove_dir_all(&staging);
        }
        staged
    } else {
        std::fs::copy(from, to).map(|_| ())
    }
}

/// Whether `a` and `b` enclose one another (one is an ancestor-or-equal of the
/// other), compared in canonical space. The staging dir may not exist yet, so its
/// parent is canonicalized and the leaf rejoined.
fn overlaps(a: &Path, b: &Path) -> bool {
    let ca = a.canonicalize().unwrap_or_else(|_| a.to_path_buf());
    let cb = match (b.parent(), b.file_name()) {
        (Some(parent), Some(leaf)) => parent
            .canonicalize()
            .unwrap_or_else(|_| parent.to_path_buf())
            .join(leaf),
        _ => b.to_path_buf(),
    };
    ca.starts_with(&cb) || cb.starts_with(&ca)
}

/// A sibling staging path for an atomic directory copy.
fn staging_path(to: &Path) -> PathBuf {
    let name = to.file_name().and_then(|n| n.to_str()).unwrap_or("skill");
    to.with_file_name(format!(".{name}.import-tmp"))
}

/// Recursively copy `from` into `to`. A *directory* symlink is skipped (no
/// follow → no loops, no escaping the tree); a *file* symlink is dereferenced and
/// copied only when its target stays inside `root` (so an in-tree symlinked
/// `SKILL.md` migrates, but a link to `~/.ssh/id_rsa` does not). `root` is the
/// resolved real source root; `from` starts at `root`.
fn copy_dir_recursive(root: &Path, from: &Path, to: &Path) -> io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let src = entry.path();
        let dst = to.join(entry.file_name());
        if file_type.is_symlink() {
            if let Ok(target) = src.canonicalize()
                && target.is_file()
                && target.starts_with(root)
            {
                std::fs::copy(&target, &dst)?;
            }
            continue;
        }
        if file_type.is_dir() {
            copy_dir_recursive(root, &src, &dst)?;
        } else {
            std::fs::copy(&src, &dst)?;
        }
    }
    Ok(())
}

/// Render `path` with a leading `~` when it is under `home`, for readable output.
fn tilde(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

/// A human-readable, multi-line preview of `plan` (for `--dry-run`, the apply
/// confirmation, and the `/import` notice).
pub fn render_preview(plan: &ImportPlan) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "stepper import → {} (global config, shared by every project — not this folder)\n",
        tilde(&plan.target_dir, &plan.home)
    ));

    if plan.sources.is_empty() {
        out.push_str("\nNo agent config detected (~/.claude, ~/.codex, ~/.cursor, ~/.gemini).\n");
        return out;
    }

    out.push_str("\nDetected:\n");
    for s in &plan.sources {
        out.push_str(&format!("  • [{}] {}\n", s.agent, s.description));
    }

    if plan.is_empty() {
        out.push_str("\nEverything is already imported — nothing to do.\n");
    } else {
        out.push_str("\nWill apply:\n");
        for s in &plan.sections {
            out.push_str(&format!("  + stepper.md  ← {}\n", s.title));
        }
        for (verdict, rule) in &plan.permission_adds {
            out.push_str(&format!("  + permission  {verdict}: {rule}\n"));
        }
        for name in &plan.mcp_adds {
            out.push_str(&format!("  + mcpServer   {name}\n"));
        }
        for (name, _) in &plan.provider_adds {
            out.push_str(&format!("  + provider    {name}\n"));
        }
        if let Some(m) = &plan.default_model {
            out.push_str(&format!("  + defaultModel {m}\n"));
        }
        for c in &plan.file_copies {
            out.push_str(&format!("  + {}  → {}\n", c.label, tilde(&c.to, &plan.home)));
        }
    }

    if !plan.notes.is_empty() {
        out.push_str("\nNotes:\n");
        for n in &plan.notes {
            out.push_str(&format!("  - {n}\n"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn convert_permission_handles_mcp_and_passthrough() {
        assert_eq!(convert_permission("mcp__pencil"), "Mcp(pencil)");
        assert_eq!(convert_permission("mcp__fs__read_file"), "Mcp(fs, read_file)");
        assert_eq!(convert_permission("mcp__server__*"), "Mcp(server)");
        // a tool with its own dunder keeps the remainder as the tool name
        assert_eq!(convert_permission("mcp__a__b__c"), "Mcp(a, b__c)");
        assert_eq!(convert_permission("Bash(git push:*)"), "Bash(git push:*)");
        assert_eq!(convert_permission("Read"), "Read");
        assert_eq!(convert_permission("  mcp__x  "), "Mcp(x)");
    }

    #[test]
    fn convert_permission_leaves_degenerate_specs_verbatim() {
        // No server segment → not turned into a rule that parses but never matches.
        assert_eq!(convert_permission("mcp__"), "mcp__");
        assert_eq!(convert_permission("mcp____tool"), "mcp____tool");
        // Trailing dunder is trimmed off the server name.
        assert_eq!(convert_permission("mcp__server__"), "Mcp(server)");
    }

    #[test]
    fn union_permission_is_idempotent() {
        let mut s = default_settings();
        assert!(union_permission(&mut s, "allow", "Mcp(pencil)"));
        assert!(!union_permission(&mut s, "allow", "Mcp(pencil)"));
        assert_eq!(s["permissions"]["allow"], serde_json::json!(["Mcp(pencil)"]));
    }

    #[test]
    fn union_is_total_on_wrong_shapes() {
        // Non-object / wrong-typed nodes are no-ops, never panics.
        let mut arr = serde_json::json!([1, 2, 3]);
        assert!(!union_permission(&mut arr, "allow", "Read"));
        assert!(!union_mcp(&mut arr, "x", serde_json::json!({})));
        let mut bad = serde_json::json!({ "permissions": [1, 2, 3] });
        assert!(!union_permission(&mut bad, "allow", "Read"));
    }

    #[test]
    fn union_mcp_keeps_existing() {
        let mut s = serde_json::json!({ "mcpServers": { "pencil": { "type": "stdio", "command": "mine" } } });
        assert!(!union_mcp(&mut s, "pencil", serde_json::json!({ "command": "theirs" })));
        assert_eq!(s["mcpServers"]["pencil"]["command"], "mine");
        assert!(union_mcp(&mut s, "other", serde_json::json!({ "type": "http" })));
    }

    #[test]
    fn validate_destination_accepts_good_rejects_bad() {
        assert!(validate_destination(&default_settings()).is_ok());
        assert!(validate_destination(&serde_json::json!({ "permissions": { "allow": [] }, "mcpServers": {} })).is_ok());
        assert!(validate_destination(&serde_json::json!([1, 2, 3])).is_err());
        assert!(validate_destination(&serde_json::json!({ "permissions": [1] })).is_err());
        assert!(validate_destination(&serde_json::json!({ "permissions": { "allow": "x" } })).is_err());
        assert!(validate_destination(&serde_json::json!({ "mcpServers": [1] })).is_err());
    }

    #[test]
    fn has_marker_line_anchors_to_whole_line() {
        let marker = "<!-- stepper-import:codex/AGENTS.md -->";
        assert!(has_marker_line(&format!("a\n{marker}\nb"), marker));
        // embedded in prose on a longer line → not a match
        assert!(!has_marker_line(&format!("see {marker} here"), marker));
    }

    #[test]
    fn codex_stdio_and_http_mapping() {
        let stdio: CodexMcpServer = toml::from_str("command = \"x\"\nargs = [\"-a\"]").unwrap();
        let mut notes = Vec::new();
        let v = stdio.to_stepper_value(&mut notes, "s");
        assert_eq!(v["type"], "stdio");
        assert_eq!(v["command"], "x");
        assert_eq!(v["args"], serde_json::json!(["-a"]));

        let http: CodexMcpServer = toml::from_str("url = \"https://h/mcp\"\nenv_http_headers = { \"X-Key\" = \"MCP_KEY\" }").unwrap();
        let v = http.to_stepper_value(&mut notes, "h");
        assert_eq!(v["type"], "http");
        assert_eq!(v["url"], "https://h/mcp");
        assert!(notes.iter().any(|n| n.contains("env-var-based auth headers")));
    }

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn end_to_end_claude_and_codex_import_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        // Claude instructions with a `~/`-anchored and a relative @import.
        write(
            &home.join(".claude/CLAUDE.md"),
            "# Rules\n@~/notes.md\n@convention/index.md\n",
        );
        write(
            &home.join(".claude/settings.json"),
            r#"{"permissions":{"allow":["mcp__pencil","Bash(cargo *)"]},"theme":"dark"}"#,
        );
        write(
            &home.join(".claude.json"),
            r#"{"mcpServers":{"pencil":{"type":"stdio","command":"px","args":["--app","cursor"],"env":{}}}}"#,
        );
        write(&home.join(".claude/skills/find-skills/SKILL.md"), "---\nname: find-skills\ndescription: d\n---\nbody");
        // Codex.
        write(
            &home.join(".codex/config.toml"),
            "personality = \"pragmatic\"\nmodel = \"gpt-5\"\nmodel_provider = \"openai\"\n[mcp_servers.local]\ncommand = \"lx\"\nargs = [\"-x\"]\n",
        );

        let plan = build_plan(home, ImportFrom::All).unwrap();
        assert!(plan.permission_adds.contains(&("allow".into(), "Mcp(pencil)".into())));
        assert!(plan.permission_adds.contains(&("allow".into(), "Bash(cargo *)".into())));
        assert!(plan.mcp_adds.iter().any(|m| m.starts_with("pencil")));
        assert!(plan.mcp_adds.iter().any(|m| m.starts_with("local")));
        assert_eq!(plan.sections.len(), 1);
        assert!(plan.file_copies.iter().any(|c| c.label.contains("find-skills")));
        // Codex model → defaultModel + a synthesized openai provider.
        assert_eq!(plan.default_model.as_deref(), Some("openai/gpt-5"));
        assert!(plan.provider_adds.iter().any(|(n, _)| n == "openai"));
        assert!(plan.notes.iter().any(|n| n.contains("Claude-only")));

        let summary = apply_plan(&plan).unwrap();
        assert_eq!(summary.sections_appended, 1);
        assert!(summary.settings_written);
        assert_eq!(summary.files_copied, 1);

        let written = std::fs::read_to_string(home.join(".stepper/setting.json")).unwrap();
        let parsed: crate::SettingsFile = serde_json::from_str(&written).unwrap();
        assert!(parsed.permissions.allow.contains(&"Mcp(pencil)".to_string()));
        assert!(parsed.mcp_servers.contains_key("pencil"));
        assert!(parsed.mcp_servers.contains_key("local"));
        assert_eq!(parsed.default_model.as_deref(), Some("openai/gpt-5"));
        assert_eq!(parsed.providers.get("openai").map(|p| p.kind.as_str()), Some("openai-compat"));
        let md = std::fs::read_to_string(home.join(".stepper/stepper.md")).unwrap();
        assert!(md.contains("Imported from Claude Code"));
        assert!(md.contains("@~/notes.md"));
        assert!(md.contains(&format!("@{}", home.join(".claude/convention/index.md").display())));
        assert!(home.join(".stepper/skills/find-skills/SKILL.md").is_file());

        // Re-running is a no-op.
        let plan2 = build_plan(home, ImportFrom::All).unwrap();
        assert!(plan2.is_empty(), "second import is empty: {:?}", plan2.sections);
        let summary2 = apply_plan(&plan2).unwrap();
        assert_eq!(summary2, ImportSummary::default());
    }

    #[test]
    fn malformed_destination_setting_aborts_instead_of_clobbering() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        // A precious destination that is genuinely unparseable even as JSONC
        // (unterminated — a trailing comma alone would now parse, since the import
        // accepts the same JSONC as `Config::load`) + real data.
        write(
            &home.join(".stepper/setting.json"),
            "{ \"step\": [\"plan\", \"mcpServers\": { \"mine\": { \"command\": \"keepme\" } }",
        );
        write(&home.join(".claude/settings.json"), r#"{"permissions":{"allow":["mcp__pencil"]}}"#);

        let err = build_plan(home, ImportFrom::All).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        // The original file is untouched (build_plan never writes).
        let still = std::fs::read_to_string(home.join(".stepper/setting.json")).unwrap();
        assert!(still.contains("keepme"));
    }

    #[test]
    fn non_object_destination_setting_aborts() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        write(&home.join(".stepper/setting.json"), "[1, 2, 3]");
        write(&home.join(".claude/settings.json"), r#"{"permissions":{"allow":["mcp__pencil"]}}"#);
        assert!(build_plan(home, ImportFrom::All).is_err());
    }

    #[test]
    fn jsonc_annotated_destination_is_accepted_by_import() {
        // A destination `setting.json` with comments / a trailing comma is valid
        // JSONC (and loads via `Config::load`), so the import must accept it, not
        // abort it as an "unreadable destination".
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        write(
            &home.join(".stepper/setting.json"),
            "{\n  // mine\n  \"mcpServers\": { \"mine\": { \"command\": \"keepme\" } },\n}",
        );
        write(&home.join(".claude/settings.json"), r#"{"permissions":{"allow":["mcp__pencil"]}}"#);
        let plan = build_plan(home, ImportFrom::All).unwrap();
        assert!(plan.permission_adds.contains(&("allow".into(), "Mcp(pencil)".into())));
    }

    #[test]
    fn malformed_source_settings_is_noted_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        write(&home.join(".claude/CLAUDE.md"), "# Rules");
        write(&home.join(".claude/settings.json"), "{ not json");
        let plan = build_plan(home, ImportFrom::All).unwrap();
        // CLAUDE.md still imports; the bad settings is a note, not an abort.
        assert_eq!(plan.sections.len(), 1);
        assert!(plan.notes.iter().any(|n| n.contains("could not parse ~/.claude/settings.json")));
        assert!(plan.permission_adds.is_empty());
    }

    #[test]
    fn marker_in_a_section_body_does_not_shadow_a_later_section() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        // CLAUDE.md body carries the AGENTS marker on its OWN line — the hardest
        // case: the CLAUDE section is appended first, and its body must not make
        // apply skip the genuine AGENTS section.
        write(
            &home.join(".claude/CLAUDE.md"),
            "rules\n<!-- stepper-import:home/AGENTS.md -->\nmore\n",
        );
        write(&home.join("AGENTS.md"), "AGENTS_UNIQUE_BODY\n");
        let plan = build_plan(home, ImportFrom::All).unwrap();
        assert_eq!(plan.sections.len(), 2, "both sections queued");
        apply_plan(&plan).unwrap();
        let md = std::fs::read_to_string(home.join(".stepper/stepper.md")).unwrap();
        assert!(md.contains("AGENTS_UNIQUE_BODY"), "AGENTS section not shadowed: {md}");
    }

    #[test]
    fn skill_with_unloadable_name_is_noted() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        // Frontmatter name has an uppercase + underscore → stepper's loader rejects it.
        write(&home.join(".claude/skills/My_Skill/SKILL.md"), "---\nname: My_Skill\ndescription: d\n---\nbody");
        let plan = build_plan(home, ImportFrom::Claude).unwrap();
        assert!(plan.file_copies.iter().any(|c| c.label.contains("My_Skill")));
        assert!(plan.notes.iter().any(|n| n.contains("may not load")));
    }

    #[test]
    fn codex_server_without_command_or_url_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        write(&home.join(".codex/config.toml"), "[mcp_servers.broken]\nenv = { K = \"v\" }\n");
        let plan = build_plan(home, ImportFrom::Codex).unwrap();
        assert!(plan.mcp_adds.is_empty());
        assert!(plan.notes.iter().any(|n| n.contains("neither command nor url")));
    }

    #[test]
    fn cross_source_mcp_collision_note_is_accurate() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        write(&home.join(".claude.json"), r#"{"mcpServers":{"pencil":{"command":"a"}}}"#);
        write(&home.join(".codex/config.toml"), "[mcp_servers.pencil]\ncommand = \"b\"\n");
        let plan = build_plan(home, ImportFrom::All).unwrap();
        assert!(plan.mcp_adds.iter().any(|m| m == "pencil (claude)"));
        assert!(plan.notes.iter().any(|n| n.contains("more than one source")));
        assert!(!plan.notes.iter().any(|n| n.contains("already in your setting.json")));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_skill_dir_is_detected_and_copied() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        // The real skill lives in a shared store; ~/.claude/skills links to it
        // (exactly how `find-skills` is laid out on disk).
        let store = home.join(".agents/skills/find-skills");
        write(&store.join("SKILL.md"), "---\nname: find-skills\ndescription: d\n---\nbody");
        std::fs::create_dir_all(home.join(".claude/skills")).unwrap();
        symlink(&store, home.join(".claude/skills/find-skills")).unwrap();

        let plan = build_plan(home, ImportFrom::Claude).unwrap();
        assert!(plan.file_copies.iter().any(|c| c.label.contains("find-skills")), "symlinked skill detected");
        apply_plan(&plan).unwrap();
        assert!(home.join(".stepper/skills/find-skills/SKILL.md").is_file(), "symlinked skill copied through");
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_in_skill_dir_are_skipped_not_followed() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let skill = home.join(".claude/skills/loopy");
        write(&skill.join("SKILL.md"), "---\nname: loopy\ndescription: d\n---\nbody");
        // A self-referential dir symlink would loop a naive recursive copy.
        symlink(&skill, skill.join("self")).unwrap();
        // A symlink to outside the source tree.
        let outside = home.join("secret.txt");
        std::fs::write(&outside, "SECRET").unwrap();
        symlink(&outside, skill.join("link.txt")).unwrap();

        let plan = build_plan(home, ImportFrom::Claude).unwrap();
        let summary = apply_plan(&plan).unwrap();
        assert_eq!(summary.files_copied, 1);
        assert!(home.join(".stepper/skills/loopy/SKILL.md").is_file());
        // Neither the loop nor the out-of-tree symlink was copied.
        assert!(!home.join(".stepper/skills/loopy/self").exists());
        assert!(!home.join(".stepper/skills/loopy/link.txt").exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_skill_md_inside_real_dir_is_copied() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        // A real skill dir whose SKILL.md is an in-tree symlink to a sibling file.
        let skill = home.join(".claude/skills/symskill");
        write(&skill.join("real-skill.md"), "---\nname: symskill\ndescription: d\n---\nbody");
        symlink(skill.join("real-skill.md"), skill.join("SKILL.md")).unwrap();

        let plan = build_plan(home, ImportFrom::Claude).unwrap();
        assert!(plan.file_copies.iter().any(|c| c.label.contains("symskill")));
        apply_plan(&plan).unwrap();
        // The symlinked SKILL.md is dereferenced and copied (so the skill loads).
        assert!(home.join(".stepper/skills/symskill/SKILL.md").is_file(), "in-tree symlinked SKILL.md copied");
    }

    #[cfg(unix)]
    #[test]
    fn skill_symlinked_to_home_ancestor_is_skipped() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        // A skill symlink that resolves to $HOME (an ancestor of ~/.stepper) would
        // make the copy recurse into its own staging dir — it must be refused.
        write(&home.join("SKILL.md"), "---\nname: evil\ndescription: d\n---\nbody");
        std::fs::write(home.join("private.txt"), "SECRET").unwrap();
        std::fs::create_dir_all(home.join(".claude/skills")).unwrap();
        symlink(home, home.join(".claude/skills/evil")).unwrap();

        let plan = build_plan(home, ImportFrom::Claude).unwrap();
        assert!(plan.file_copies.is_empty(), "ancestor symlink not queued for copy");
        assert!(plan.notes.iter().any(|n| n.contains("overlaps ~/.stepper")));
        // apply is a clean no-op for this skill — no self-recursion, nothing leaks.
        apply_plan(&plan).unwrap();
        assert!(!home.join(".stepper/skills/evil/private.txt").exists());
    }

    #[cfg(unix)]
    #[test]
    fn dangling_destination_symlink_is_kept_not_overwritten() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        write(&home.join(".claude/skills/foo/SKILL.md"), "---\nname: foo\ndescription: d\n---\nbody");
        // The destination is a broken symlink (e.g. a moved-away shared store).
        std::fs::create_dir_all(home.join(".stepper/skills")).unwrap();
        symlink(home.join("gone"), home.join(".stepper/skills/foo")).unwrap();

        let plan = build_plan(home, ImportFrom::Claude).unwrap();
        // Treated as already-present: not queued, noted, and apply doesn't error.
        assert!(plan.file_copies.is_empty());
        assert!(plan.notes.iter().any(|n| n.contains("already exists")));
        apply_plan(&plan).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn skill_symlinked_into_stepper_skills_is_skipped() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        // A skill symlink pointing *into* ~/.stepper/skills (a descendant of
        // ~/.stepper, not an ancestor) — the staging dir would be its own child.
        write(&home.join(".stepper/skills/SKILL.md"), "---\nname: x\ndescription: d\n---\nbody");
        std::fs::create_dir_all(home.join(".claude/skills")).unwrap();
        symlink(home.join(".stepper/skills"), home.join(".claude/skills/foo")).unwrap();

        let plan = build_plan(home, ImportFrom::Claude).unwrap();
        assert!(plan.file_copies.is_empty(), "overlapping skill not queued");
        assert!(plan.notes.iter().any(|n| n.contains("overlaps ~/.stepper")));
        // No self-recursion / ENAMETOOLONG.
        apply_plan(&plan).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn skill_md_symlinked_outside_dir_is_skipped_with_note() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        // Real skill dir whose SKILL.md links OUTSIDE the dir → copy would drop it.
        let skill = home.join(".claude/skills/shared");
        std::fs::create_dir_all(&skill).unwrap();
        write(&home.join(".claude/shared-SKILL.md"), "---\nname: shared\ndescription: d\n---\nbody");
        symlink(home.join(".claude/shared-SKILL.md"), skill.join("SKILL.md")).unwrap();
        std::fs::write(skill.join("data.txt"), "d").unwrap();

        let plan = build_plan(home, ImportFrom::Claude).unwrap();
        assert!(plan.file_copies.is_empty(), "skill with out-of-tree SKILL.md not queued");
        assert!(plan.notes.iter().any(|n| n.contains("links outside the skill dir")));
        apply_plan(&plan).unwrap();
        assert!(!home.join(".stepper/skills/shared").exists(), "no half-broken skill written");
    }

    #[test]
    fn dotted_skill_dir_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        // A source dir named like staging must never be seen as a skill.
        write(&home.join(".claude/skills/.foo.import-tmp/SKILL.md"), "---\nname: foo\ndescription: d\n---\nb");
        write(&home.join(".claude/skills/foo/SKILL.md"), "---\nname: foo\ndescription: d\n---\nb");
        let plan = build_plan(home, ImportFrom::Claude).unwrap();
        assert_eq!(plan.file_copies.len(), 1, "only the real skill is queued");
        assert!(plan.file_copies.iter().all(|c| c.label.contains("'foo'")));
        let summary = apply_plan(&plan).unwrap();
        assert_eq!(summary.files_copied, 1);
        assert!(home.join(".stepper/skills/foo/SKILL.md").is_file());
    }

    #[test]
    fn no_sources_yields_empty_plan_and_preview() {
        let dir = tempfile::tempdir().unwrap();
        let plan = build_plan(dir.path(), ImportFrom::All).unwrap();
        assert!(plan.is_empty());
        assert!(plan.sources.is_empty());
        assert!(render_preview(&plan).contains("No agent config detected"));
    }

    #[test]
    fn from_filter_limits_sources() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        write(&home.join(".claude/CLAUDE.md"), "rules");
        write(&home.join(".codex/config.toml"), "[mcp_servers.x]\ncommand = \"c\"\n");
        write(&home.join(".cursorrules"), "cursor rules");
        write(&home.join(".gemini/GEMINI.md"), "gemini rules");

        let claude_only = build_plan(home, ImportFrom::Claude).unwrap();
        assert!(claude_only.sources.iter().all(|s| s.agent == "claude"));
        let codex_only = build_plan(home, ImportFrom::Codex).unwrap();
        assert!(codex_only.sources.iter().all(|s| s.agent == "codex"));
        let cursor_only = build_plan(home, ImportFrom::Cursor).unwrap();
        assert!(
            !cursor_only.sources.is_empty()
                && cursor_only.sources.iter().all(|s| s.agent == "cursor")
        );
        let gemini_only = build_plan(home, ImportFrom::Gemini).unwrap();
        assert!(
            !gemini_only.sources.is_empty()
                && gemini_only.sources.iter().all(|s| s.agent == "gemini")
        );
        // `All` includes every detected source.
        let all = build_plan(home, ImportFrom::All).unwrap();
        let agents: std::collections::HashSet<_> = all.sources.iter().map(|s| s.agent).collect();
        assert!(agents.contains("cursor") && agents.contains("gemini"));
    }

    #[test]
    fn cursor_import_collects_cursorrules_and_rules() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        write(&home.join(".cursorrules"), "top-level rules");
        write(&home.join(".cursor/rules/foo.md"), "foo rule");
        write(&home.join(".cursor/rules/bar.mdc"), "bar rule");

        let plan = build_plan(home, ImportFrom::Cursor).unwrap();
        let markers: Vec<&str> = plan.sections.iter().map(|s| s.marker.as_str()).collect();
        assert!(markers.contains(&"<!-- stepper-import:cursor/.cursorrules -->"));
        assert!(markers.contains(&"<!-- stepper-import:cursor/rules/foo -->"));
        assert!(markers.contains(&"<!-- stepper-import:cursor/rules/bar -->"), "`.mdc` is included");
        assert!(plan.sources.iter().all(|s| s.agent == "cursor"));

        apply_plan(&plan).unwrap();
        let md = std::fs::read_to_string(home.join(".stepper/stepper.md")).unwrap();
        assert!(md.contains("top-level rules") && md.contains("foo rule") && md.contains("bar rule"));
        // Second run is a no-op (marker idempotency).
        let again = build_plan(home, ImportFrom::Cursor).unwrap();
        assert!(again.sections.is_empty(), "re-import skips already-imported sections");
    }

    #[test]
    fn gemini_import_collects_global_gemini_md() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        write(&home.join(".gemini/GEMINI.md"), "gemini instructions");

        let plan = build_plan(home, ImportFrom::Gemini).unwrap();
        assert_eq!(plan.sections.len(), 1);
        assert_eq!(plan.sections[0].marker, "<!-- stepper-import:gemini/GEMINI.md -->");
        assert!(plan.sources.iter().all(|s| s.agent == "gemini"));

        apply_plan(&plan).unwrap();
        let md = std::fs::read_to_string(home.join(".stepper/stepper.md")).unwrap();
        assert!(md.contains("gemini instructions"));
        let again = build_plan(home, ImportFrom::Gemini).unwrap();
        assert!(again.sections.is_empty());
    }

    #[test]
    fn import_from_parse() {
        assert_eq!(ImportFrom::parse("claude"), Some(ImportFrom::Claude));
        assert_eq!(ImportFrom::parse("CODEX"), Some(ImportFrom::Codex));
        assert_eq!(ImportFrom::parse("cursor"), Some(ImportFrom::Cursor));
        assert_eq!(ImportFrom::parse("GEMINI"), Some(ImportFrom::Gemini));
        assert_eq!(ImportFrom::parse(""), Some(ImportFrom::All));
        assert_eq!(ImportFrom::parse("nope"), None);
    }

    #[test]
    fn provider_entry_mirrors_convention_provider() {
        let (openai, amb) = provider_entry("openai");
        assert_eq!(openai["kind"], "openai-compat");
        assert_eq!(openai["baseUrl"], "https://api.openai.com/v1");
        assert!(openai.get("apiKey").is_none(), "key resolves from STEPPER_*_API_KEY");
        assert!(!amb);

        let (codex, _) = provider_entry("codex");
        assert_eq!(codex["kind"], "openai-responses");
        assert_eq!(codex["auth"], "codex-oauth");

        let (anthropic, _) = provider_entry("anthropic");
        assert_eq!(anthropic["kind"], "anthropic");
        assert!(anthropic.get("baseUrl").is_none());

        let (other, amb) = provider_entry("my-llm");
        assert_eq!(other["kind"], "openai-compat");
        assert!(amb, "an off-convention name is flagged for a manual base URL/key");
    }

    #[test]
    fn union_provider_and_default_model_keep_existing_and_are_total() {
        let mut s = default_settings();
        assert!(union_provider(&mut s, "openai", serde_json::json!({"kind": "openai-compat"})));
        assert!(!union_provider(&mut s, "openai", serde_json::json!({"kind": "anthropic"})), "keep-existing");
        assert_eq!(s["providers"]["openai"]["kind"], "openai-compat");
        assert!(set_default_model_if_absent(&mut s, "openai/gpt-5"));
        assert!(!set_default_model_if_absent(&mut s, "anthropic/x"), "never clobbers");
        assert_eq!(s["defaultModel"], "openai/gpt-5");
        // Total on a non-object.
        let mut bad = serde_json::json!([1, 2]);
        assert!(!union_provider(&mut bad, "x", Value::Null));
        assert!(!set_default_model_if_absent(&mut bad, "x/y"));
    }

    #[test]
    fn claude_model_synthesizes_anthropic_default_and_provider() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        write(
            &home.join(".claude/settings.json"),
            r#"{"model":"claude-sonnet-4-6","permissions":{"allow":["Bash(ls)"]}}"#,
        );
        let plan = build_plan(home, ImportFrom::Claude).unwrap();
        assert_eq!(plan.default_model.as_deref(), Some("anthropic/claude-sonnet-4-6"));
        let (name, cfg) = plan.provider_adds.iter().find(|(n, _)| n == "anthropic").unwrap();
        assert_eq!(name, "anthropic");
        assert_eq!(cfg["kind"], "anthropic");
        // The model key is consumed, so the "Claude-only settings" note must not fire.
        assert!(!plan.notes.iter().any(|n| n.contains("Claude-only")));
    }

    #[test]
    fn existing_default_model_is_not_overwritten_but_provider_is_added() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        // Pre-existing destination with a user defaultModel and no providers.
        write(
            &home.join(".stepper/setting.json"),
            r#"{"defaultModel":"ollama-cloud/qwen3-coder"}"#,
        );
        write(
            &home.join(".codex/config.toml"),
            "model = \"gpt-5\"\nmodel_provider = \"openai\"\n",
        );
        let plan = build_plan(home, ImportFrom::Codex).unwrap();
        // defaultModel kept (not applied), but the provider is still synthesized.
        assert!(plan.default_model.is_none(), "destination defaultModel wins");
        assert!(plan.provider_adds.iter().any(|(n, _)| n == "openai"));
        assert!(plan.notes.iter().any(|n| n.contains("kept your existing defaultModel")));

        apply_plan(&plan).unwrap();
        let parsed: crate::SettingsFile =
            serde_json::from_str(&std::fs::read_to_string(home.join(".stepper/setting.json")).unwrap()).unwrap();
        assert_eq!(parsed.default_model.as_deref(), Some("ollama-cloud/qwen3-coder"));
        assert!(parsed.providers.contains_key("openai"));
    }

    #[test]
    fn all_collects_both_models_claude_wins_default_both_providers_added() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        write(&home.join(".claude/settings.json"), r#"{"model":"claude-opus-4-8"}"#);
        write(
            &home.join(".codex/config.toml"),
            "model = \"gpt-5\"\nmodel_provider = \"openai\"\n",
        );
        let plan = build_plan(home, ImportFrom::All).unwrap();
        // Claude is collected first → first-writer wins the defaultModel.
        assert_eq!(plan.default_model.as_deref(), Some("anthropic/claude-opus-4-8"));
        let names: Vec<&str> = plan.provider_adds.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"anthropic") && names.contains(&"openai"), "both providers: {names:?}");
        assert!(plan.notes.iter().any(|n| n.contains("kept the first imported defaultModel")));
    }
}
