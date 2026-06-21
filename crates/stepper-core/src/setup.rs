use crate::layer::{FailurePolicy, StepDef};
use std::collections::BTreeMap;
use stepper_config::{parse_layer, parse_skill, Config, FormatterConfig};
use stepper_tools::{builtin_formatters, Detect, Formatter};

/// Per-layer ReAct step backstop for the implicit `default` layer. Set high so it
/// never interrupts legitimate long tasks — the user-facing runaway guard is the
/// opt-in time/budget `limits` (none by default). It still caps a truly stuck
/// model rather than looping forever. A custom layer overrides via `steps:`.
const DEFAULT_STEP_CAP: usize = 250;
/// Default upper bound on concurrent workers for a `parallel` layer when its
/// frontmatter does not set `parallel-max`.
const DEFAULT_PARALLEL_MAX: usize = 8;

/// The default layer's role/identity (the "what you are and what you work on").
/// Universal *behavior* lives in [`AGENT_DIRECTIVES`] and is prepended by
/// [`compose_system`] for every layer, so this stays a short role description.
pub const DEFAULT_SYSTEM_PROMPT: &str = "You are stepper, a CLI software-engineering agent working in the user's project. \
Your tools include read_file, write_file, edit_file, bash, grep, glob, list_dir, todo_write, and web_fetch. \
Prefer reading before editing, keep changes minimal and idiomatic, and match the conventions of the surrounding code.";

/// The universal agentic behavior contract, prepended to every layer / sub-agent
/// system message by [`compose_system`]. Written in plain imperative English with
/// no vendor-specific tags, so it steers weak open models (e.g. the default
/// `qwen3-coder`) and strong hosted ones alike. Its primary job is to stop the
/// "narrate-then-stop" failure — where the model says what it will do and ends
/// the turn without doing it.
///
/// Distilled from published, cross-vendor agentic-prompting guidance:
/// - OpenAI GPT-4.1 / 5.1 prompting guides — Persistence ("keep going until the
///   query is completely resolved … only terminate when sure the problem is
///   solved"), Tool-calling ("use your tools … do NOT guess or make up an
///   answer"), and Planning ("plan … and reflect … on the outcomes") reminders,
///   reported to lift SWE-bench Verified by up to ~20%.
/// - Anthropic Claude prompting best practices — complete tasks fully, never
///   artificially stop early.
/// - Google Gemini prompting strategies — plan before acting; persist.
/// - Open-model (Qwen / Llama / BFCL) tool-use docs — emit the actual tool call
///   rather than narrating it; one call at a time; wait for the real result.
pub const AGENT_DIRECTIVES: &str = "You are an autonomous agent that works across many steps within a single turn — not a chatbot that answers once and waits. On every step, follow these rules:\n\
\n\
- Keep going until done. Continue working until the user's request is fully resolved. Do not end your turn until the work is actually finished and verified, or you genuinely need information that only the user can provide. Only stop when you are sure the task is solved. Never wind down early because the task is large, is taking many steps, or your context is filling up.\n\
- Act, do not just announce. Never end your turn right after stating what you intend to do next. If you say you are about to do something (for example \"now I'll create the file\"), make the tool call to do it in that same step. Announcing an action and then yielding without performing it is a failure — do the thing.\n\
- Use tools instead of guessing. If you are unsure about a file's contents, the project's structure, or any fact you can check, use a tool to read or search for it. Never invent file contents, APIs, command output, or results, and never speculate about code you have not opened — read it first.\n\
- Plan briefly, then reflect. Think through the request and the steps it needs before acting, and after each tool result consider what it means before the next step. Do not try to solve the whole task by blindly chaining tool calls with no reasoning.\n\
- One action at a time. Take a single action per step and wait for its real result before the next. You only request a tool call; the system runs it and returns the result — never imagine or assume a tool's output, and never continue as if a call you only described had already run.\n\
- Bias toward action. If the request is clear enough to act on, act on it: make the most reasonable assumption, proceed, and note it afterward rather than stopping to ask. Only ask the user when something is genuinely ambiguous or the choice truly matters for safety or correctness. Do not over-use tools for trivial questions you can answer directly.\n\
- Verify your work. After changing code, check it with the project's build or tests where possible and fix what you broke.\n\
- Finish cleanly. Only once the task is complete (or you truly need the user) do you end the turn, with a short, concrete summary of what you did.";

/// Compose a complete system message shared by every execution path: the
/// universal [`AGENT_DIRECTIVES`] first (so the behavior contract is prominent),
/// then the project base context, then the layer/sub-agent's own role prompt.
/// Main layers, parallel workers, and dispatched sub-agents all build their
/// system message here so they behave consistently.
pub fn compose_system(base_context: &str, role_prompt: &str) -> String {
    let mut out = String::from(AGENT_DIRECTIVES);
    for part in [base_context.trim(), role_prompt.trim()] {
        if !part.is_empty() {
            out.push_str("\n\n");
            out.push_str(part);
        }
    }
    out
}

/// The directories strictly below `root` down to `cwd` (inclusive), topmost
/// first — the chain whose `CLAUDE.md` files accumulate onto the project base.
/// Empty when `cwd` is `root` itself or not within it.
fn subdir_chain(root: &std::path::Path, cwd: &std::path::Path) -> Vec<std::path::PathBuf> {
    let Ok(rel) = cwd.strip_prefix(root) else {
        return Vec::new();
    };
    let mut dir = root.to_path_buf();
    let mut out = Vec::new();
    for comp in rel.components() {
        dir = dir.join(comp);
        out.push(dir.clone());
    }
    out
}

/// Whether a path-scoped rule with these `paths` globs applies at `rel_cwd` (the
/// working dir relative to the project root, `/`-normalized, `""` at the root).
/// No globs → always. A glob is a directory scope: a trailing `/**` or `/*` (or a
/// bare directory) matches that directory and everything beneath it; `*`/`**`
/// match everywhere.
fn rule_applies(paths: &[String], rel_cwd: &str) -> bool {
    if paths.is_empty() {
        return true;
    }
    paths.iter().any(|p| {
        let p = p.trim().trim_matches('/');
        if p.is_empty() || p == "*" || p == "**" {
            return true;
        }
        // Reduce a `dir/**`, `dir/*`, or bare `dir` glob to its directory prefix,
        // then match the cwd as that directory or any descendant of it.
        let base = p.trim_end_matches("**").trim_end_matches('*').trim_end_matches('/');
        !base.is_empty() && (rel_cwd == base || rel_cwd.starts_with(&format!("{base}/")))
    })
}

/// The base/pinned context. The PROJECT base is first-found-wins: stepper's own
/// config takes precedence (project `.stepper/stepper.md`, then user
/// `~/.stepper/stepper.md`); a local `CLAUDE.md` is the fallback for users who
/// haven't migrated — project root `./CLAUDE.md`, then global `~/.claude/CLAUDE.md`
/// (opencode-style: project beats global). ON TOP of that, Claude-Code-style
/// hierarchical `CLAUDE.md` files in every directory from below the project root
/// down to `cwd` are accumulated (most specific last), so a subtree can add its
/// own rules. Project memory is appended after. Empty when none exists.
pub fn load_base_context(config: &Config, cwd: &std::path::Path) -> String {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    // (file, base dir for `@import` resolution), in precedence order.
    let mut candidates: Vec<(std::path::PathBuf, std::path::PathBuf)> = Vec::new();
    if let Some(dir) = config.project_dir.as_ref() {
        candidates.push((dir.join("stepper.md"), dir.clone()));
    }
    if let Some(root) = config.project_root.as_ref() {
        candidates.push((root.join("CLAUDE.md"), root.clone()));
    }
    if let Some(dir) = config.user_dir.as_ref() {
        candidates.push((dir.join("stepper.md"), dir.clone()));
    }
    if let Some(claude) = home.as_ref().map(|h| h.join(".claude")) {
        candidates.push((claude.join("CLAUDE.md"), claude));
    }
    let mut context = String::new();
    for (file, base) in candidates {
        if let Ok(text) = std::fs::read_to_string(&file) {
            // Resolve `@import` directives (Claude-Code-style) — relative to the
            // file's own dir, `~/` to $HOME — so a CLAUDE.md that pulls in shared
            // rule files keeps working.
            context = stepper_config::imports::resolve_imports(&text, &base, home.as_deref());
            break;
        }
    }
    // Hierarchical accumulation: append each subdirectory's `CLAUDE.md` from below
    // the project root down to `cwd` (most specific last). The root's own
    // `CLAUDE.md` is left to the first-found base above (so a migrated user with
    // `.stepper/stepper.md` doesn't double-load it).
    if let Some(root) = config.project_root.as_ref() {
        for dir in subdir_chain(root, cwd) {
            if let Ok(text) = std::fs::read_to_string(dir.join("CLAUDE.md")) {
                let resolved = stepper_config::imports::resolve_imports(&text, &dir, home.as_deref());
                if !resolved.trim().is_empty() {
                    if !context.is_empty() {
                        context.push_str("\n\n");
                    }
                    context.push_str(&resolved);
                }
            }
        }
    }
    // Path-scoped rules (`.stepper/rules/*.md`): each carries an optional `paths:`
    // glob list (relative to the project root). A rule with no `paths` always
    // applies; otherwise it loads only when `cwd` matches one of its globs — so a
    // subtree's conventions don't burden unrelated work.
    if let Some(rules_dir) = config.project_dir.as_ref().map(|d| d.join("rules"))
        && let Ok(entries) = std::fs::read_dir(&rules_dir)
    {
        let rel_cwd = config
            .project_root
            .as_ref()
            .and_then(|root| cwd.strip_prefix(root).ok())
            .map(|r| r.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        let mut files: Vec<std::path::PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("md"))
            .collect();
        files.sort();
        for file in files {
            if let Ok(text) = std::fs::read_to_string(&file)
                && let Ok(rule) = stepper_config::parse_rule(&text)
                && rule_applies(&rule.paths, &rel_cwd)
            {
                let resolved =
                    stepper_config::imports::resolve_imports(&rule.body, &rules_dir, home.as_deref());
                if !resolved.trim().is_empty() {
                    if !context.is_empty() {
                        context.push_str("\n\n");
                    }
                    context.push_str(&resolved);
                }
            }
        }
    }
    // Project memory (written by the agent via `memory_write`) is ADDITIVE: it is
    // always appended to whichever pinned context won above, so a future session
    // sees the learnings the agent recorded — the auto-memory loop.
    if let Some(root) = config.project_root.as_ref()
        && let Ok(raw) = std::fs::read_to_string(root.join(crate::MEMORY_REL_PATH))
    {
        // The memory file is append-only and grows across sessions; cap what loads
        // into every prompt by keeping the most-recent tail (it can't bloat the
        // context window or cost unbounded).
        const MEMORY_MAX_BYTES: usize = 32 * 1024;
        let capped = if raw.len() > MEMORY_MAX_BYTES {
            let cut = raw.len() - MEMORY_MAX_BYTES;
            let cut = (cut..raw.len()).find(|&i| raw.is_char_boundary(i)).unwrap_or(raw.len());
            format!("(older memory truncated)\n{}", &raw[cut..])
        } else {
            raw
        };
        let text = capped.trim();
        if !text.is_empty() {
            if !context.is_empty() {
                context.push_str("\n\n");
            }
            context.push_str(text);
        }
    }
    context
}

/// Map a reasoning-effort level to the per-dialect controls: OpenAI-family
/// `reasoning_effort` string AND an Anthropic extended-thinking token budget, so
/// one `/effort` knob drives "how hard to think" regardless of provider.
pub fn effort_controls(level: &str) -> (Option<String>, Option<u32>) {
    match level.trim().to_ascii_lowercase().as_str() {
        "low" => (Some("low".into()), Some(2_048)),
        "medium" | "med" => (Some("medium".into()), Some(8_192)),
        "high" => (Some("high".into()), Some(16_384)),
        // `xhigh`/`max` are Anthropic effort levels (adaptive thinking). The
        // string flows to `output_config.effort` on modern Claude and clamps to
        // `high` for OpenAI; the budget is only a legacy-model fallback.
        "xhigh" => (Some("xhigh".into()), Some(24_576)),
        "max" => (Some("max".into()), Some(32_768)),
        // "off"/unknown → no reasoning override, no thinking budget.
        _ => (None, None),
    }
}

/// Resolve `settings.formatter` into the active formatter set for format-on-edit.
/// Omitted / `false` → none (the default); `true` → every built-in; a map keeps
/// built-ins on while applying per-formatter overrides (`disabled`/`command`/
/// `extensions`/`environment`) and adding custom formatters (a `command` +
/// `extensions` under an unknown name). Mirrors opencode's `format` resolution.
pub fn resolve_formatters(cfg: Option<&FormatterConfig>) -> Vec<Formatter> {
    match cfg {
        None | Some(FormatterConfig::All(false)) => Vec::new(),
        Some(FormatterConfig::All(true)) => builtin_formatters(),
        Some(FormatterConfig::Map(map)) => {
            let mut by_name: BTreeMap<String, Formatter> = builtin_formatters()
                .into_iter()
                .map(|f| (f.name.clone(), f))
                .collect();
            for (name, entry) in map {
                if entry.disabled {
                    by_name.remove(name);
                    continue;
                }
                match by_name.get_mut(name) {
                    // Override a built-in in place.
                    Some(f) => {
                        if let Some(exts) = &entry.extensions {
                            f.extensions = exts.clone();
                        }
                        for (k, v) in &entry.environment {
                            f.environment.insert(k.clone(), v.clone());
                        }
                        if let Some(cmd) = &entry.command {
                            f.detect = Detect::Command {
                                command: cmd.clone(),
                            };
                        }
                    }
                    // A custom formatter needs both a command and extensions.
                    None => {
                        if let (Some(cmd), Some(exts)) = (&entry.command, &entry.extensions) {
                            by_name.insert(
                                name.clone(),
                                Formatter {
                                    name: name.clone(),
                                    extensions: exts.clone(),
                                    environment: entry.environment.clone(),
                                    detect: Detect::Command {
                                        command: cmd.clone(),
                                    },
                                },
                            );
                        }
                    }
                }
            }
            by_name.into_values().collect()
        }
    }
}

/// Build the `step` pipeline from config. With no `step`, a single implicit
/// `default` layer runs the default model with every tool. A global
/// `settings.reasoningEffort` (or `--effort`) fills each step's reasoning
/// controls where the layer's own frontmatter did not set them.
pub fn build_steps(config: &Config, default_model: &str) -> Vec<StepDef> {
    let mut steps = if config.settings.step.is_empty() {
        let model_ref = config
            .orchestrator_model()
            .unwrap_or_else(|| default_model.to_string());
        vec![StepDef {
            name: "default".into(),
            model_ref,
            system_prompt: DEFAULT_SYSTEM_PROMPT.into(),
            tool_allow: Vec::new(),
            tool_deny: Vec::new(),
            mcp_allow: Vec::new(),
            step_cap: DEFAULT_STEP_CAP,
            color: None,
            on_failure: FailurePolicy::Stop,
            retries: 0,
            temperature: config.settings.orchestrator.as_ref().and_then(|o| o.temperature),
            top_p: None,
            reasoning_effort: None,
            thinking_budget: None,
            permission: Vec::new(),
            parallel: false,
            parallel_max: DEFAULT_PARALLEL_MAX,
            skills: Vec::new(),
        }]
    } else {
        config
            .settings
            .step
            .clone()
            .into_iter()
            .map(|name| build_step(config, &name, default_model))
            .collect()
    };

    // Global effort fills unset reasoning controls (per-layer frontmatter wins).
    if let Some(level) = config.settings.reasoning_effort.as_deref() {
        let (re, tb) = effort_controls(level);
        for step in &mut steps {
            if step.reasoning_effort.is_none() {
                step.reasoning_effort = re.clone();
            }
            if step.thinking_budget.is_none() {
                step.thinking_budget = tb;
            }
        }
    }
    steps
}

fn build_step(config: &Config, name: &str, default_model: &str) -> StepDef {
    let layer = load_layer_index(config, name);

    let model_ref = layer
        .as_ref()
        .and_then(layer_model_ref)
        .or_else(|| config.layer_model(name))
        .unwrap_or_else(|| default_model.to_string());

    match layer {
        Some(layer) => {
            // Load the layer's skills and advertise them (name + description) in
            // the prompt; the full body is loaded on demand via the `skill` tool.
            let skills = load_skills(config, &layer.frontmatter.skills);
            let system_prompt =
                format!("{}{}", layer.system_prompt, crate::skills::advertise(&skills));
            StepDef {
                name: name.to_string(),
                model_ref,
                system_prompt,
                tool_allow: layer.frontmatter.tools.allow,
                tool_deny: layer.frontmatter.tools.deny,
                mcp_allow: layer.frontmatter.mcp.allow,
                step_cap: layer.frontmatter.steps.map(|s| s as usize).unwrap_or(DEFAULT_STEP_CAP),
                color: layer.frontmatter.color,
                on_failure: FailurePolicy::parse(layer.frontmatter.on_failure.as_deref()),
                retries: layer.frontmatter.retries as usize,
                temperature: layer
                    .frontmatter
                    .temperature
                    .or_else(|| config.settings.orchestrator.as_ref().and_then(|o| o.temperature)),
                top_p: layer.frontmatter.top_p,
                reasoning_effort: layer.frontmatter.reasoning_effort,
                thinking_budget: layer.frontmatter.thinking_budget,
                permission: layer.frontmatter.permission.into_iter().collect(),
                parallel: layer.frontmatter.parallel,
                parallel_max: layer
                    .frontmatter
                    .parallel_max
                    .map(|n| (n as usize).max(1))
                    .unwrap_or(DEFAULT_PARALLEL_MAX),
                skills,
            }
        }
        None => StepDef {
            name: name.to_string(),
            model_ref,
            system_prompt: DEFAULT_SYSTEM_PROMPT.into(),
            tool_allow: Vec::new(),
            tool_deny: Vec::new(),
            mcp_allow: Vec::new(),
            step_cap: DEFAULT_STEP_CAP,
            color: None,
            on_failure: FailurePolicy::Stop,
            retries: 0,
            temperature: None,
            top_p: None,
            reasoning_effort: None,
            thinking_budget: None,
            permission: Vec::new(),
            parallel: false,
            parallel_max: DEFAULT_PARALLEL_MAX,
            skills: Vec::new(),
        },
    }
}

/// Compile a named agent into a one-shot [`StepDef`] for the `#<agent>` prompt
/// trigger (run as the turn's sole layer). Uses the agent's model/tools/role; the
/// caller restores the original pipeline afterwards.
pub fn agent_step(agent: &crate::layer::AgentDef, default_model: &str) -> StepDef {
    StepDef {
        name: agent.name.clone(),
        model_ref: agent
            .model_ref
            .clone()
            .unwrap_or_else(|| default_model.to_string()),
        system_prompt: agent.role_prompt.clone(),
        tool_allow: agent.tool_allow.clone(),
        tool_deny: agent.tool_deny.clone(),
        mcp_allow: Vec::new(),
        step_cap: DEFAULT_STEP_CAP,
        color: None,
        on_failure: FailurePolicy::Stop,
        retries: 0,
        temperature: None,
        top_p: None,
        reasoning_effort: None,
        thinking_budget: None,
        permission: Vec::new(),
        parallel: false,
        parallel_max: DEFAULT_PARALLEL_MAX,
        skills: Vec::new(),
    }
}

/// Load named sub-agents from `.stepper/agents/<name>/index.md` (project first,
/// then `~/.stepper/agents/`; project wins). Each is parsed with the same layer
/// frontmatter as `step` layers. Returns them sorted by name.
pub fn load_agents(config: &Config) -> Vec<crate::layer::AgentDef> {
    use std::collections::BTreeMap;
    let mut by_name: BTreeMap<String, crate::layer::AgentDef> = BTreeMap::new();
    for dir in [config.project_dir.as_ref(), config.user_dir.as_ref()]
        .into_iter()
        .flatten()
    {
        let Ok(entries) = std::fs::read_dir(dir.join("agents")) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !is_safe_component(&name) || by_name.contains_key(&name) {
                continue;
            }
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let path = entry.path().join("index.md");
            if let Ok(content) = std::fs::read_to_string(&path)
                && let Ok(layer) = parse_layer(&name, &content)
            {
                by_name.insert(
                    name.clone(),
                    crate::layer::AgentDef {
                        description: layer.frontmatter.description.clone().unwrap_or_default(),
                        model_ref: layer_model_ref(&layer),
                        tool_allow: layer.frontmatter.tools.allow.clone(),
                        tool_deny: layer.frontmatter.tools.deny.clone(),
                        role_prompt: layer.system_prompt.clone(),
                        name,
                    },
                );
            }
        }
    }
    by_name.into_values().collect()
}

fn layer_model_ref(layer: &stepper_config::LayerDef) -> Option<String> {
    match (&layer.frontmatter.model, &layer.frontmatter.provider) {
        (Some(m), _) if m.contains('/') => Some(m.clone()),
        (Some(m), Some(p)) => Some(format!("{p}/{m}")),
        _ => None,
    }
}

/// Load a layer's referenced skills (each `.stepper/skills/<name>/SKILL.md`),
/// preserving order and skipping any that are missing or unsafe-named. The bodies
/// ride on the `StepDef` and are served on demand by the `skill` tool; only the
/// name + description are advertised in the prompt (progressive disclosure).
fn load_skills(config: &Config, skills: &[String]) -> Vec<stepper_config::SkillDef> {
    skills.iter().filter_map(|name| load_skill(config, name)).collect()
}

/// A name is only used as a single path component, so reject any separators or
/// traversal — a malicious `.stepper/` must not read outside its own tree.
fn is_safe_component(name: &str) -> bool {
    !name.is_empty()
        && name == name.trim()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains("..")
}

fn load_skill(config: &Config, name: &str) -> Option<stepper_config::SkillDef> {
    if !is_safe_component(name) {
        return None;
    }
    for dir in [config.project_dir.as_ref(), config.user_dir.as_ref()]
        .into_iter()
        .flatten()
    {
        let path = dir.join("skills").join(name).join("SKILL.md");
        if let Ok(content) = std::fs::read_to_string(&path)
            && let Ok(skill) = parse_skill(&content)
        {
            return Some(skill);
        }
    }
    None
}

fn load_layer_index(config: &Config, name: &str) -> Option<stepper_config::LayerDef> {
    if !is_safe_component(name) {
        return None;
    }
    for dir in [config.project_dir.as_ref(), config.user_dir.as_ref()]
        .into_iter()
        .flatten()
    {
        let path = dir.join("layer").join(name).join("index.md");
        if let Ok(content) = std::fs::read_to_string(&path)
            && let Ok(layer) = parse_layer(name, &content)
        {
            return Some(layer);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_context_reads_project_root_claude_md_as_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let stepper_dir = root.join(".stepper");
        std::fs::create_dir_all(&stepper_dir).unwrap();
        std::fs::write(root.join("CLAUDE.md"), "PROJECT CLAUDE RULES").unwrap();

        let mut cfg = Config::from_settings(Default::default());
        cfg.project_dir = Some(stepper_dir);
        cfg.project_root = Some(root);
        // No stepper.md anywhere → falls back to the project-root CLAUDE.md.
        let ctx = load_base_context(&cfg, cfg.project_root.as_deref().unwrap());
        assert!(ctx.contains("PROJECT CLAUDE RULES"), "reads ./CLAUDE.md fallback: {ctx}");
    }

    #[test]
    fn base_context_appends_project_memory_additively() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let stepper_dir = root.join(".stepper");
        std::fs::create_dir_all(stepper_dir.join("memory")).unwrap();
        std::fs::write(stepper_dir.join("stepper.md"), "PINNED CONTEXT").unwrap();
        std::fs::write(
            stepper_dir.join("memory/MEMORY.md"),
            "# Project memory\n\n- run cargo test\n",
        )
        .unwrap();

        let mut cfg = Config::from_settings(Default::default());
        cfg.project_dir = Some(stepper_dir);
        cfg.project_root = Some(root);
        let ctx = load_base_context(&cfg, cfg.project_root.as_deref().unwrap());
        // The pinned context wins the first-found candidate, and the agent's memory
        // is appended after it (the auto-memory reload).
        assert!(ctx.contains("PINNED CONTEXT"), "keeps the pinned context: {ctx}");
        assert!(ctx.contains("- run cargo test"), "appends project memory: {ctx}");
    }

    #[test]
    fn base_context_caps_an_oversized_memory_file_to_the_recent_tail() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let stepper_dir = root.join(".stepper");
        let mem_dir = stepper_dir.join("memory");
        std::fs::create_dir_all(&mem_dir).unwrap();
        // A small pinned context wins first-found (so the test doesn't pick up the
        // real ~/.claude/CLAUDE.md fallback), and the memory below is appended.
        std::fs::write(stepper_dir.join("stepper.md"), "BASE").unwrap();
        // ~200KB of bullets, far over the 32KB load cap; the newest must survive.
        let mut body = String::from("# Project memory\n\n");
        for i in 0..8_000 {
            body.push_str(&format!("- old learning number {i}\n"));
        }
        body.push_str("- NEWEST learning sentinel\n");
        std::fs::write(mem_dir.join("MEMORY.md"), &body).unwrap();

        let mut cfg = Config::from_settings(Default::default());
        cfg.project_dir = Some(stepper_dir);
        cfg.project_root = Some(root);
        let ctx = load_base_context(&cfg, cfg.project_root.as_deref().unwrap());
        assert!(ctx.len() < 64 * 1024, "the loaded memory is bounded, not {}B", ctx.len());
        assert!(ctx.starts_with("BASE"), "pinned context kept first");
        assert!(ctx.contains("(older memory truncated)"), "marks the truncation: tail-only");
        assert!(ctx.contains("NEWEST learning sentinel"), "keeps the most recent tail");
        assert!(!ctx.contains("old learning number 0"), "drops the oldest entries");
    }

    #[test]
    fn effort_controls_maps_levels_to_reasoning_and_thinking() {
        assert_eq!(effort_controls("off"), (None, None));
        assert_eq!(effort_controls("low"), (Some("low".into()), Some(2_048)));
        assert_eq!(effort_controls("high"), (Some("high".into()), Some(16_384)));
        // Anthropic effort levels flow through as the string; the budget is only a
        // legacy-model fallback.
        assert_eq!(effort_controls("xhigh"), (Some("xhigh".into()), Some(24_576)));
        assert_eq!(effort_controls("max"), (Some("max".into()), Some(32_768)));
        assert_eq!(effort_controls("bogus"), (None, None), "unknown → off");
    }

    #[test]
    fn build_steps_applies_global_effort_to_unset_layers() {
        let settings = stepper_config::SettingsFile {
            reasoning_effort: Some("medium".into()),
            ..Default::default()
        };
        let cfg = Config::from_settings(settings);
        let steps = build_steps(&cfg, "p/m");
        assert_eq!(steps[0].reasoning_effort.as_deref(), Some("medium"), "global effort fills the step");
        assert_eq!(steps[0].thinking_budget, Some(8_192));
        // No global effort → unset (provider default).
        let bare = build_steps(&Config::from_settings(Default::default()), "p/m");
        assert!(bare[0].reasoning_effort.is_none() && bare[0].thinking_budget.is_none());
    }

    #[test]
    fn base_context_accumulates_subdirectory_claude_md_from_root_to_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let stepper_dir = root.join(".stepper");
        let sub = root.join("src").join("widgets");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir_all(&stepper_dir).unwrap();
        std::fs::write(stepper_dir.join("stepper.md"), "PROJECT BASE").unwrap();
        std::fs::write(root.join("src").join("CLAUDE.md"), "SRC RULES").unwrap();
        std::fs::write(sub.join("CLAUDE.md"), "WIDGET RULES").unwrap();

        let mut cfg = Config::from_settings(Default::default());
        cfg.project_dir = Some(stepper_dir);
        cfg.project_root = Some(root.clone());
        // From cwd = root/src/widgets, both subdir CLAUDE.md files accumulate onto
        // the project base, most specific last.
        let ctx = load_base_context(&cfg, &sub);
        assert!(ctx.contains("PROJECT BASE"), "project base kept: {ctx}");
        let src_at = ctx.find("SRC RULES").expect("src CLAUDE.md loaded");
        let widget_at = ctx.find("WIDGET RULES").expect("widget CLAUDE.md loaded");
        assert!(src_at < widget_at, "topmost subdir first, most specific last: {ctx}");
        // From the root itself, no subdir files are pulled in.
        let at_root = load_base_context(&cfg, &root);
        assert!(!at_root.contains("SRC RULES"), "no subdir accumulation at the root: {at_root}");
    }

    #[test]
    fn rule_applies_matches_directory_scopes() {
        assert!(rule_applies(&[], "anywhere"), "no globs → always");
        assert!(rule_applies(&["**".into()], "src/widgets"));
        assert!(rule_applies(&["src".into()], "src"), "bare dir matches itself");
        assert!(rule_applies(&["src".into()], "src/widgets"), "and descendants");
        assert!(rule_applies(&["src/**".into()], "src/a/b"));
        assert!(!rule_applies(&["src".into()], "tests"));
        assert!(!rule_applies(&["src".into()], "srcfoo"), "prefix must be a path boundary");
        assert!(rule_applies(&["a".into(), "b".into()], "b/x"), "any glob matches");
    }

    #[test]
    fn path_scoped_rules_load_only_when_cwd_matches() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let rules = root.join(".stepper").join("rules");
        let sub = root.join("src");
        std::fs::create_dir_all(&rules).unwrap();
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(rules.join("always.md"), "ALWAYS RULE").unwrap();
        std::fs::write(rules.join("src.md"), "---\npaths: src\n---\nSRC RULE").unwrap();

        let mut cfg = Config::from_settings(Default::default());
        cfg.project_dir = Some(root.join(".stepper"));
        cfg.project_root = Some(root.clone());
        // At the root: only the unscoped rule loads.
        let at_root = load_base_context(&cfg, &root);
        assert!(at_root.contains("ALWAYS RULE"), "unscoped rule always loads: {at_root}");
        assert!(!at_root.contains("SRC RULE"), "src-scoped rule not loaded at root: {at_root}");
        // Inside src/: both load.
        let in_src = load_base_context(&cfg, &sub);
        assert!(in_src.contains("ALWAYS RULE") && in_src.contains("SRC RULE"), "{in_src}");
    }

    #[test]
    fn base_context_prefers_stepper_md_over_claude_md() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let stepper_dir = root.join(".stepper");
        std::fs::create_dir_all(&stepper_dir).unwrap();
        std::fs::write(stepper_dir.join("stepper.md"), "STEPPER WINS").unwrap();
        std::fs::write(root.join("CLAUDE.md"), "CLAUDE FALLBACK").unwrap();

        let mut cfg = Config::from_settings(Default::default());
        cfg.project_dir = Some(stepper_dir);
        cfg.project_root = Some(root);
        let ctx = load_base_context(&cfg, cfg.project_root.as_deref().unwrap());
        assert!(ctx.contains("STEPPER WINS"), "stepper.md wins over CLAUDE.md: {ctx}");
        assert!(!ctx.contains("CLAUDE FALLBACK"), "CLAUDE.md unused when stepper.md exists");
    }

    #[test]
    fn compose_system_prepends_directives_and_orders_parts() {
        let s = compose_system("PROJECT RULES", "ROLE PROMPT");
        // The behavior contract leads, then base context, then the role prompt.
        let dir = s.find("autonomous agent").unwrap();
        let base = s.find("PROJECT RULES").unwrap();
        let role = s.find("ROLE PROMPT").unwrap();
        assert!(dir < base && base < role, "order directives → base → role: {s}");
        // The narrate-then-stop fix is present.
        assert!(s.contains("Act, do not just announce"));
        assert!(s.contains("Keep going until done"));
    }

    #[test]
    fn compose_system_skips_empty_parts() {
        // No base context (the common case: a project with no stepper.md).
        let s = compose_system("", DEFAULT_SYSTEM_PROMPT);
        assert!(s.starts_with(AGENT_DIRECTIVES));
        assert!(s.contains("You are stepper"));
        assert!(!s.contains("\n\n\n"), "no blank gap from an empty base: {s:?}");

        // Directives only (empty base + empty role) — still valid, no trailing gap.
        let bare = compose_system("  ", "");
        assert_eq!(bare, AGENT_DIRECTIVES);
    }
}
