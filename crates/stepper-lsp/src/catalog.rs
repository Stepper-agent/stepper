//! The built-in language-server catalog (detected on `PATH`) plus the resolved
//! [`ServerSpec`] the manager spawns. Servers are never downloaded; only ones
//! already installed are used. `stepper-core` filters/overrides this from
//! `settings.lsp` and may add custom servers.

use serde_json::Value;
use std::path::PathBuf;

/// A built-in server entry: the binary to look for on `PATH`, the argv to launch
/// it (typically `--stdio`), and the file extensions it handles.
#[derive(Debug, Clone)]
pub struct BuiltinServer {
    pub id: &'static str,
    pub bin: &'static str,
    pub args: &'static [&'static str],
    pub extensions: &'static [&'static str],
}

/// A fully resolved server ready to spawn (binary located, argv built).
#[derive(Debug, Clone)]
pub struct ServerSpec {
    pub id: String,
    /// argv (program + args); the program is an absolute path for built-ins.
    pub command: Vec<String>,
    pub extensions: Vec<String>,
    pub env: Vec<(String, String)>,
    pub initialization: Option<Value>,
}

impl ServerSpec {
    pub fn handles(&self, file_name: &str) -> bool {
        self.extensions
            .iter()
            .any(|ext| file_name.ends_with(ext.as_str()))
    }
}

/// The built-in catalog (a representative, commonly-installed set). Each is used
/// only if its `bin` is found on `PATH`.
pub fn builtin_catalog() -> &'static [BuiltinServer] {
    &[
        BuiltinServer { id: "rust-analyzer", bin: "rust-analyzer", args: &[], extensions: &[".rs"] },
        BuiltinServer { id: "gopls", bin: "gopls", args: &[], extensions: &[".go"] },
        BuiltinServer {
            id: "pyright",
            bin: "pyright-langserver",
            args: &["--stdio"],
            extensions: &[".py", ".pyi"],
        },
        BuiltinServer {
            id: "typescript",
            bin: "typescript-language-server",
            args: &["--stdio"],
            extensions: &[".ts", ".tsx", ".mts", ".cts", ".js", ".jsx", ".mjs", ".cjs"],
        },
        BuiltinServer {
            id: "clangd",
            bin: "clangd",
            args: &[],
            extensions: &[".c", ".cc", ".cpp", ".cxx", ".c++", ".h", ".hh", ".hpp", ".hxx"],
        },
        BuiltinServer { id: "zls", bin: "zls", args: &[], extensions: &[".zig", ".zon"] },
        BuiltinServer {
            id: "lua",
            bin: "lua-language-server",
            args: &[],
            extensions: &[".lua"],
        },
        BuiltinServer {
            id: "bash",
            bin: "bash-language-server",
            args: &["start"],
            extensions: &[".sh", ".bash"],
        },
        BuiltinServer {
            id: "yaml",
            bin: "yaml-language-server",
            args: &["--stdio"],
            extensions: &[".yaml", ".yml"],
        },
        BuiltinServer {
            id: "json",
            bin: "vscode-json-language-server",
            args: &["--stdio"],
            extensions: &[".json", ".jsonc"],
        },
    ]
}

/// Locate `bin` on `PATH` (or treat an explicit path as-is). Returns the absolute
/// program path so the spawned `Command` doesn't re-search.
pub fn which(bin: &str) -> Option<PathBuf> {
    if bin.contains('/') {
        let p = PathBuf::from(bin);
        return p.is_file().then_some(p);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        let candidate = dir.join(bin);
        candidate.is_file().then_some(candidate)
    })
}

/// Resolve a built-in by id to a [`ServerSpec`] if its binary is installed.
pub fn resolve_builtin(b: &BuiltinServer) -> Option<ServerSpec> {
    let program = which(b.bin)?;
    let mut command = vec![program.to_string_lossy().into_owned()];
    command.extend(b.args.iter().map(|s| s.to_string()));
    Some(ServerSpec {
        id: b.id.to_string(),
        command,
        extensions: b.extensions.iter().map(|s| s.to_string()).collect(),
        env: Vec::new(),
        initialization: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn which_finds_sh_and_misses_garbage() {
        assert!(which("sh").is_some());
        assert!(which("definitely-not-a-real-lsp-binary-xyz").is_none());
    }

    #[test]
    fn catalog_entries_have_extensions_and_unique_ids() {
        let cat = builtin_catalog();
        let mut ids = std::collections::HashSet::new();
        for b in cat {
            assert!(!b.extensions.is_empty(), "{} has no extensions", b.id);
            assert!(ids.insert(b.id), "duplicate id {}", b.id);
        }
        assert!(cat.iter().any(|b| b.id == "rust-analyzer"));
    }

    #[test]
    fn server_spec_matches_by_extension_suffix() {
        let spec = ServerSpec {
            id: "x".into(),
            command: vec!["x".into()],
            extensions: vec![".rs".into()],
            env: Vec::new(),
            initialization: None,
        };
        assert!(spec.handles("main.rs"));
        assert!(!spec.handles("main.go"));
    }
}
