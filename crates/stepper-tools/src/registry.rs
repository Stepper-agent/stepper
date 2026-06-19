use crate::tools::{bash, fetch, files, search, todo};
use crate::Tool;
use std::collections::BTreeMap;
use std::sync::Arc;
use stepper_provider::ToolSpec;

/// A named set of tools. The base registry holds every built-in; a per-layer
/// view is a filtered clone (cheap `Arc` shares).
#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Every built-in tool.
    pub fn builtins() -> Self {
        let mut r = Self::new();
        r.register(Arc::new(files::ReadFile::default()));
        r.register(Arc::new(files::WriteFile::default()));
        r.register(Arc::new(files::EditFile::default()));
        r.register(Arc::new(bash::Bash::default()));
        r.register(Arc::new(search::Grep::default()));
        r.register(Arc::new(search::GlobTool::default()));
        r.register(Arc::new(search::ListDir::default()));
        r.register(Arc::new(todo::TodoWrite::default()));
        r.register(Arc::new(fetch::WebFetch::default()));
        r
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    pub fn names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.values().map(|t| t.spec().clone()).collect()
    }

    /// A layer's view: if `allow` is non-empty, keep only those tools; then drop
    /// anything in `deny` (deny wins). Empty `allow` means "inherit all".
    pub fn filtered(&self, allow: &[String], deny: &[String]) -> ToolRegistry {
        let tools = self
            .tools
            .iter()
            .filter(|(name, _)| allow.is_empty() || allow.iter().any(|a| a == *name))
            .filter(|(name, _)| !deny.iter().any(|d| d == *name))
            .map(|(name, tool)| (name.clone(), tool.clone()))
            .collect();
        ToolRegistry { tools }
    }

    /// Scope MCP tools to a layer's `mcp.allow`: keep all non-`mcp__` tools, and
    /// only `mcp__<server>__*` whose server is allowed. Empty `allowed_servers`
    /// inherits all. `always_load_servers` (config `alwaysLoad: true`) stay
    /// visible to every layer even when not in the layer's allow-list.
    pub fn filter_mcp(
        &self,
        allowed_servers: &[String],
        always_load_servers: &[String],
    ) -> ToolRegistry {
        if allowed_servers.is_empty() {
            return self.clone();
        }
        // MCP tools are namespaced with the SANITIZED server name (stepper-mcp
        // `bridge::sanitize`), so the scope prefix must sanitize identically —
        // otherwise a server name with `.`/space/`:` never matches and its tools
        // are silently scoped out (or, for a disallowed name, leak in).
        let prefixes: Vec<String> = allowed_servers
            .iter()
            .chain(always_load_servers.iter())
            .map(|s| format!("mcp__{}__", sanitize_mcp_segment(s)))
            .collect();
        let tools = self
            .tools
            .iter()
            .filter(|(name, _)| {
                !name.starts_with("mcp__") || prefixes.iter().any(|p| name.starts_with(p))
            })
            .map(|(name, tool)| (name.clone(), tool.clone()))
            .collect();
        ToolRegistry { tools }
    }
}

/// Mirror of the MCP tool-name sanitizer (stepper-mcp `bridge::sanitize`): a
/// server name is namespaced into tool names with every non-[A-Za-z0-9_-] char
/// replaced by `_`. Kept here (not imported) because stepper-mcp depends on
/// stepper-tools, not the reverse.
fn sanitize_mcp_segment(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Tool, ToolCx};
    use async_trait::async_trait;
    use serde_json::Value;
    use stepper_provider::{ToolError, ToolResult, ToolSpec};

    struct Named(ToolSpec);
    #[async_trait]
    impl Tool for Named {
        fn spec(&self) -> &ToolSpec {
            &self.0
        }
        async fn call(&self, _args: Value, _cx: &ToolCx) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::text("ok"))
        }
    }

    fn registry_with(names: &[&str]) -> ToolRegistry {
        let mut r = ToolRegistry::new();
        for n in names {
            r.register(Arc::new(Named(ToolSpec {
                name: (*n).into(),
                description: String::new(),
                input_schema: serde_json::json!({ "type": "object" }),
                read_only: false,
                parallel_safe: false,
            })));
        }
        r
    }

    #[test]
    fn filter_mcp_empty_allow_inherits_all() {
        let names = registry_with(&["read_file", "mcp__alpha__x", "mcp__beta__y"])
            .filter_mcp(&[], &[])
            .names();
        assert_eq!(names.len(), 3);
    }

    #[test]
    fn filter_mcp_scopes_to_allowed_servers() {
        let names = registry_with(&["read_file", "mcp__alpha__x", "mcp__beta__y"])
            .filter_mcp(&["alpha".into()], &[])
            .names();
        assert!(names.contains(&"read_file".to_string()), "non-mcp tools always kept");
        assert!(names.contains(&"mcp__alpha__x".to_string()));
        assert!(!names.contains(&"mcp__beta__y".to_string()), "beta is not allowed");
    }

    #[test]
    fn filter_mcp_scopes_special_char_server_via_sanitized_prefix() {
        // A server keyed "my.server" namespaces its tools as `mcp__my_server__*`;
        // the allow-list must sanitize identically or the match silently fails.
        let names = registry_with(&["read_file", "mcp__my_server__x", "mcp__other__y"])
            .filter_mcp(&["my.server".into()], &[])
            .names();
        assert!(
            names.contains(&"mcp__my_server__x".to_string()),
            "allowed special-char server matches via sanitized prefix"
        );
        assert!(!names.contains(&"mcp__other__y".to_string()));
    }

    #[test]
    fn always_load_server_survives_layer_scoping() {
        let names = registry_with(&["mcp__alpha__x", "mcp__beta__y", "mcp__ctx__z"])
            .filter_mcp(&["alpha".into()], &["ctx".into()])
            .names();
        assert!(names.contains(&"mcp__alpha__x".to_string()), "allowed server kept");
        assert!(
            names.contains(&"mcp__ctx__z".to_string()),
            "alwaysLoad server stays visible even though the layer did not allow it"
        );
        assert!(
            !names.contains(&"mcp__beta__y".to_string()),
            "a server that is neither allowed nor alwaysLoad is dropped"
        );
    }
}
