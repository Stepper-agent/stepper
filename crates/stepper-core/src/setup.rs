use crate::layer::{FailurePolicy, StepDef};
use stepper_config::{parse_layer, parse_skill, Config};

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

/// The base/pinned context (`.stepper/stepper.md`, project over user). Empty when
/// none exists.
pub fn load_base_context(config: &Config) -> String {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    for dir in [config.project_dir.as_ref(), config.user_dir.as_ref()]
        .into_iter()
        .flatten()
    {
        if let Ok(text) = std::fs::read_to_string(dir.join("stepper.md")) {
            // Resolve `@import` directives (Claude-Code-style) — relative to the
            // `.stepper/` dir, `~/` to $HOME — so a migrated CLAUDE.md that pulls
            // in shared rule files keeps working.
            return stepper_config::imports::resolve_imports(&text, dir, home.as_deref());
        }
    }
    String::new()
}

/// Build the `step` pipeline from config. With no `step`, a single implicit
/// `default` layer runs the default model with every tool.
pub fn build_steps(config: &Config, default_model: &str) -> Vec<StepDef> {
    if config.settings.step.is_empty() {
        let model_ref = config
            .orchestrator_model()
            .unwrap_or_else(|| default_model.to_string());
        return vec![StepDef {
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
        }];
    }

    config
        .settings
        .step
        .clone()
        .into_iter()
        .map(|name| build_step(config, &name, default_model))
        .collect()
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
