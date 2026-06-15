use crate::layer::{FailurePolicy, StepDef};
use stepper_config::{parse_layer, parse_skill, Config};

const DEFAULT_STEP_CAP: usize = 40;
/// Default upper bound on concurrent workers for a `parallel` layer when its
/// frontmatter does not set `parallel-max`.
const DEFAULT_PARALLEL_MAX: usize = 8;

pub const DEFAULT_SYSTEM_PROMPT: &str = "You are stepper, a CLI software-engineering agent. \
Work in the user's project using the provided tools (read_file, write_file, edit_file, bash, \
grep, glob, list_dir, todo_write, web_fetch). Prefer reading before editing, keep changes \
minimal and idiomatic, and verify with the build/tests when possible. When the task is done, \
give a short summary of what you changed.";

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
