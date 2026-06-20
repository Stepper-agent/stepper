use crate::tools::{apply_patch, bash, fetch, files, search, todo};
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
        r.register(Arc::new(apply_patch::ApplyPatch::default()));
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
        // Scope by tool ORIGIN (its MCP server), not by string prefix on the
        // namespaced name: `mcp__<server>__<tool>` cannot be parsed back into
        // (server, tool) once either contains `__`, so a prefix match leaks a
        // server whose sanitized name is a prefix of another (`alpha` vs
        // `alpha__beta`). Server names are sanitized identically to how the bridge
        // namespaces them, so a `.`/space/`:` name still matches.
        let allowed: std::collections::HashSet<String> = allowed_servers
            .iter()
            .chain(always_load_servers.iter())
            .map(|s| sanitize_mcp_segment(s))
            .collect();
        let tools = self
            .tools
            .iter()
            .filter(|(_, tool)| match tool.mcp_server() {
                None => true, // built-in tools are never MCP-scoped
                Some(server) => allowed.contains(&sanitize_mcp_segment(server)),
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

    struct Named {
        spec: ToolSpec,
        server: Option<String>,
    }
    #[async_trait]
    impl Tool for Named {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }
        fn mcp_server(&self) -> Option<&str> {
            self.server.as_deref()
        }
        async fn call(&self, _args: Value, _cx: &ToolCx) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::text("ok"))
        }
    }

    fn named(name: &str, server: Option<&str>) -> Arc<Named> {
        Arc::new(Named {
            spec: ToolSpec {
                name: name.into(),
                description: String::new(),
                input_schema: serde_json::json!({ "type": "object" }),
                read_only: false,
                parallel_safe: false,
            },
            server: server.map(String::from),
        })
    }

    /// Derive the server for the simple `mcp__<server>__<tool>` test names. Real
    /// `McpTool` carries its server explicitly — this is only for the legacy
    /// single-`__`-segment fixtures below; ambiguous names use `named(..)` directly.
    fn registry_with(names: &[&str]) -> ToolRegistry {
        let mut r = ToolRegistry::new();
        for n in names {
            let server = n
                .strip_prefix("mcp__")
                .map(|rest| rest.split("__").next().unwrap_or("").to_string());
            r.register(named(n, server.as_deref()));
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
    fn filter_mcp_does_not_leak_a_superstring_server() {
        // Two distinct servers whose sanitized names are prefix-related: "alpha"
        // and "alpha__beta". The old `name.starts_with("mcp__alpha__")` prefix
        // match leaked the latter into a layer scoped only to "alpha"; origin-based
        // scoping keys on the tool's real server, so it does not.
        let mut r = ToolRegistry::new();
        r.register(named("mcp__alpha__x", Some("alpha")));
        r.register(named("mcp__alpha__beta__y", Some("alpha__beta")));
        let names = r.filter_mcp(&["alpha".into()], &[]).names();
        assert!(names.contains(&"mcp__alpha__x".to_string()), "allowed server's tool kept");
        assert!(
            !names.contains(&"mcp__alpha__beta__y".to_string()),
            "a different server is NOT leaked by a shared name prefix"
        );
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
