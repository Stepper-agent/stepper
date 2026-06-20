//! Built-in code formatters and the format-on-edit runner.
//!
//! A [`Formatter`] knows the file extensions it handles, optional environment, and
//! a [`Detect`] strategy that decides — at format time, against the project — both
//! *whether* the formatter is available and *what command* to run (its argv
//! contains a `$FILE` placeholder). [`builtin_formatters`] is the catalog
//! (mirroring opencode's `format/formatter.ts`); `stepper-core` filters/overrides
//! it from `settings.formatter` and runs [`format_file`] after a file-editing tool
//! succeeds. The runner is best-effort: a missing binary or a non-zero exit is
//! skipped silently (an editor formatter must never fail the edit).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// A resolved formatter: which files it handles, env to run it with, and how to
/// detect/build its command.
#[derive(Clone, Debug)]
pub struct Formatter {
    pub name: String,
    /// Filename suffixes (e.g. `.rs`, `.html.erb`); matched against the file name.
    pub extensions: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub detect: Detect,
}

/// How a formatter decides availability and produces its command (with `$FILE`).
#[derive(Clone, Debug)]
pub enum Detect {
    /// Available iff `bin` is on `PATH`; runs `[bin, ..args, $FILE]`.
    Which { bin: String, args: Vec<String> },
    /// Available iff `bin` is on `PATH` **and** one of `config_files` exists
    /// walking up from the edited file to the project root.
    WhichWithConfig {
        bin: String,
        args: Vec<String>,
        config_files: Vec<String>,
    },
    /// Available iff a `manifest` (e.g. `package.json`) found walking up declares
    /// `dep` in its dependency maps **and** `bin` resolves (PATH or
    /// `node_modules/.bin`); runs `[resolved_bin, ..args, $FILE]`.
    Manifest {
        manifest: String,
        dep: String,
        bin: String,
        args: Vec<String>,
    },
    /// A user-supplied command (always "available"); its argv must contain `$FILE`.
    Command { command: Vec<String> },
}

fn which(bin: &str) -> Option<PathBuf> {
    if bin.contains('/') {
        let p = PathBuf::from(bin);
        return p.is_file().then_some(p);
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(bin);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Walk from `start` up to and including `stop`, returning the first ancestor that
/// contains `name`.
fn find_up(name: &str, start: &Path, stop: &Path) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        let candidate = d.join(name);
        if candidate.exists() {
            return Some(candidate);
        }
        if d == stop {
            break;
        }
        dir = d.parent();
    }
    None
}

/// A locally installed JS bin (`node_modules/.bin/<bin>`) walking up to the root.
fn node_bin(bin: &str, start: &Path, stop: &Path) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        let candidate = d.join("node_modules").join(".bin").join(bin);
        if candidate.is_file() {
            return Some(candidate);
        }
        if d == stop {
            break;
        }
        dir = d.parent();
    }
    None
}

fn manifest_has_dep(manifest_path: &Path, dep: &str) -> bool {
    let Ok(text) = std::fs::read_to_string(manifest_path) else {
        return false;
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };
    ["dependencies", "devDependencies", "require", "require-dev"]
        .iter()
        .filter_map(|k| json.get(*k))
        .filter_map(|v| v.as_object())
        .any(|m| m.contains_key(dep))
}

/// Resolve a formatter's command for `file_dir` within `project_root`, or `None`
/// when it isn't available here.
fn resolve_command(f: &Formatter, file_dir: &Path, project_root: &Path) -> Option<Vec<String>> {
    let build = |bin: String, args: &[String]| {
        let mut cmd = vec![bin];
        cmd.extend(args.iter().cloned());
        cmd
    };
    match &f.detect {
        Detect::Which { bin, args } => {
            let resolved = which(bin)?;
            Some(build(resolved.to_string_lossy().into_owned(), args))
        }
        Detect::WhichWithConfig {
            bin,
            args,
            config_files,
        } => {
            let resolved = which(bin)?;
            config_files
                .iter()
                .find_map(|c| find_up(c, file_dir, project_root))?;
            Some(build(resolved.to_string_lossy().into_owned(), args))
        }
        Detect::Manifest {
            manifest,
            dep,
            bin,
            args,
        } => {
            let m = find_up(manifest, file_dir, project_root)?;
            if !manifest_has_dep(&m, dep) {
                return None;
            }
            let resolved = node_bin(bin, file_dir, project_root)
                .or_else(|| which(bin))?
                .to_string_lossy()
                .into_owned();
            Some(build(resolved, args))
        }
        Detect::Command { command } => (!command.is_empty()).then(|| command.clone()),
    }
}

/// Run every enabled formatter that matches `path`'s extension. Best-effort:
/// unavailable formatters and non-zero exits are skipped. Returns the names that
/// actually ran (for the tool result / tests). `path` should be absolute.
pub async fn format_file(path: &Path, project_root: &Path, formatters: &[Formatter]) -> Vec<String> {
    if formatters.is_empty() {
        return Vec::new();
    }
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return Vec::new();
    };
    let file_dir = path.parent().unwrap_or(project_root);
    let file = path.to_string_lossy().into_owned();

    let mut ran = Vec::new();
    for f in formatters {
        if !f.extensions.iter().any(|ext| name.ends_with(ext.as_str())) {
            continue;
        }
        let Some(cmd) = resolve_command(f, file_dir, project_root) else {
            continue;
        };
        let argv: Vec<String> = cmd
            .iter()
            .map(|part| part.replace("$FILE", &file))
            .collect();
        let mut command = tokio::process::Command::new(&argv[0]);
        command
            .args(&argv[1..])
            .current_dir(project_root)
            .envs(&f.environment)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        // A formatter that can't even spawn (race on the binary) is skipped, like
        // a non-zero exit — never surfaced as an edit failure.
        if command.status().await.is_ok() {
            ran.push(f.name.clone());
        }
    }
    ran
}

fn fmt(name: &str, exts: &[&str], detect: Detect) -> Formatter {
    Formatter {
        name: name.into(),
        extensions: exts.iter().map(|s| s.to_string()).collect(),
        environment: BTreeMap::new(),
        detect,
    }
}

fn which_detect(bin: &str, args: &[&str]) -> Detect {
    Detect::Which {
        bin: bin.into(),
        args: args.iter().map(|s| s.to_string()).collect(),
    }
}

/// The built-in formatter catalog (a faithful port of opencode's set). `$FILE` is
/// substituted with the edited file's path at run time.
pub fn builtin_formatters() -> Vec<Formatter> {
    let env_be_bun = || {
        let mut e = BTreeMap::new();
        e.insert("BUN_BE_BUN".to_string(), "1".to_string());
        e
    };
    vec![
        fmt("gofmt", &[".go"], which_detect("gofmt", &["-w", "$FILE"])),
        fmt("rustfmt", &[".rs"], which_detect("rustfmt", &["$FILE"])),
        fmt(
            "mix",
            &[".ex", ".exs", ".eex", ".heex", ".leex", ".neex", ".sface"],
            which_detect("mix", &["format", "$FILE"]),
        ),
        fmt("zig", &[".zig", ".zon"], which_detect("zig", &["fmt", "$FILE"])),
        fmt("ktlint", &[".kt", ".kts"], which_detect("ktlint", &["-F", "$FILE"])),
        fmt("dart", &[".dart"], which_detect("dart", &["format", "$FILE"])),
        fmt("dfmt", &[".d"], which_detect("dfmt", &["-i", "$FILE"])),
        fmt(
            "terraform",
            &[".tf", ".tfvars"],
            which_detect("terraform", &["fmt", "$FILE"]),
        ),
        fmt("gleam", &[".gleam"], which_detect("gleam", &["format", "$FILE"])),
        fmt("nixfmt", &[".nix"], which_detect("nixfmt", &["$FILE"])),
        fmt("ormolu", &[".hs"], which_detect("ormolu", &["-i", "$FILE"])),
        fmt(
            "cljfmt",
            &[".clj", ".cljs", ".cljc", ".edn"],
            which_detect("cljfmt", &["fix", "--quiet", "$FILE"]),
        ),
        fmt(
            "rubocop",
            &[".rb", ".rake", ".gemspec", ".ru"],
            which_detect("rubocop", &["--autocorrect", "$FILE"]),
        ),
        fmt(
            "standardrb",
            &[".rb", ".rake", ".gemspec", ".ru"],
            which_detect("standardrb", &["--fix", "$FILE"]),
        ),
        fmt(
            "htmlbeautifier",
            &[".erb", ".html.erb"],
            which_detect("htmlbeautifier", &["$FILE"]),
        ),
        fmt("shfmt", &[".sh", ".bash"], which_detect("shfmt", &["-w", "$FILE"])),
        fmt("latexindent", &[".tex"], which_detect("latexindent", &["-w", "-s", "$FILE"])),
        fmt("uv", &[".py", ".pyi"], which_detect("uv", &["format", "--", "$FILE"])),
        fmt("air", &[".R"], which_detect("air", &["format", "$FILE"])),
        fmt(
            "clang-format",
            &[
                ".c", ".cc", ".cpp", ".cxx", ".c++", ".h", ".hh", ".hpp", ".hxx", ".h++", ".ino",
                ".C", ".H",
            ],
            Detect::WhichWithConfig {
                bin: "clang-format".into(),
                args: vec!["-i".into(), "$FILE".into()],
                config_files: vec![".clang-format".into()],
            },
        ),
        fmt(
            "ocamlformat",
            &[".ml", ".mli"],
            Detect::WhichWithConfig {
                bin: "ocamlformat".into(),
                args: vec!["-i".into(), "$FILE".into()],
                config_files: vec![".ocamlformat".into()],
            },
        ),
        fmt(
            "ruff",
            &[".py", ".pyi"],
            Detect::WhichWithConfig {
                bin: "ruff".into(),
                args: vec!["format".into(), "$FILE".into()],
                config_files: vec!["pyproject.toml".into(), "ruff.toml".into(), ".ruff.toml".into()],
            },
        ),
        Formatter {
            name: "prettier".into(),
            extensions: [
                ".js", ".jsx", ".mjs", ".cjs", ".ts", ".tsx", ".mts", ".cts", ".html", ".htm",
                ".css", ".scss", ".sass", ".less", ".vue", ".svelte", ".json", ".jsonc", ".yaml",
                ".yml", ".toml", ".xml", ".md", ".mdx", ".graphql", ".gql",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            environment: env_be_bun(),
            detect: Detect::Manifest {
                manifest: "package.json".into(),
                dep: "prettier".into(),
                bin: "prettier".into(),
                args: vec!["--write".into(), "$FILE".into()],
            },
        },
        Formatter {
            name: "biome".into(),
            extensions: [
                ".js", ".jsx", ".mjs", ".cjs", ".ts", ".tsx", ".mts", ".cts", ".html", ".css",
                ".json", ".jsonc",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            environment: env_be_bun(),
            detect: Detect::Manifest {
                manifest: "package.json".into(),
                dep: "@biomejs/biome".into(),
                bin: "biome".into(),
                args: vec!["format".into(), "--write".into(), "$FILE".into()],
            },
        },
        Formatter {
            name: "pint".into(),
            extensions: vec![".php".into()],
            environment: BTreeMap::new(),
            detect: Detect::Manifest {
                manifest: "composer.json".into(),
                dep: "laravel/pint".into(),
                bin: "pint".into(),
                args: vec!["$FILE".into()],
            },
        },
    ]
    .into_iter()
    .filter(|f| !f.extensions.is_empty())
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn catalog_covers_common_languages_and_is_well_formed() {
        let cat = builtin_formatters();
        for lang in [".rs", ".go", ".py", ".ts", ".rb", ".c"] {
            assert!(
                cat.iter().any(|f| f.extensions.iter().any(|e| e == lang)),
                "no formatter for {lang}"
            );
        }
        // Every detect command carries the $FILE placeholder.
        for f in &cat {
            let argv = match &f.detect {
                Detect::Which { args, .. }
                | Detect::WhichWithConfig { args, .. }
                | Detect::Manifest { args, .. } => args.clone(),
                Detect::Command { command } => command.clone(),
            };
            assert!(
                argv.iter().any(|a| a.contains("$FILE")),
                "{} has no $FILE",
                f.name
            );
            assert!(!f.extensions.is_empty(), "{} has no extensions", f.name);
        }
    }

    #[tokio::test]
    async fn format_file_runs_a_custom_command_and_substitutes_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("x.demo");
        std::fs::write(&target, "unformatted").unwrap();

        // A "formatter" that rewrites the file in place via a portable shell.
        let formatters = vec![Formatter {
            name: "demo".into(),
            extensions: vec![".demo".into()],
            environment: BTreeMap::new(),
            detect: Detect::Command {
                command: vec![
                    "sh".into(),
                    "-c".into(),
                    "printf formatted > \"$1\"".into(),
                    "sh".into(),
                    "$FILE".into(),
                ],
            },
        }];

        let ran = format_file(&target, dir.path(), &formatters).await;
        assert_eq!(ran, vec!["demo".to_string()]);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "formatted");
    }

    #[tokio::test]
    async fn format_file_skips_non_matching_extensions() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("x.other");
        std::fs::write(&target, "keep").unwrap();
        let formatters = vec![Formatter {
            name: "demo".into(),
            extensions: vec![".demo".into()],
            environment: BTreeMap::new(),
            detect: Detect::Command {
                command: vec!["sh".into(), "-c".into(), "printf changed > \"$1\"".into(), "sh".into(), "$FILE".into()],
            },
        }];
        let ran = format_file(&target, dir.path(), &formatters).await;
        assert!(ran.is_empty());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep");
    }

    #[test]
    fn catalog_matches_opencode_argv_details() {
        let cat = builtin_formatters();
        let find = |name: &str| cat.iter().find(|f| f.name == name).expect(name).clone();

        // cljfmt runs `fix --quiet $FILE` (opencode formatter.ts) — the `--quiet`
        // flag suppresses its per-file progress noise.
        let cljfmt = find("cljfmt");
        let Detect::Which { args, .. } = &cljfmt.detect else {
            panic!("cljfmt should be a Which detector");
        };
        assert_eq!(args, &["fix", "--quiet", "$FILE"]);

        // clang-format also handles the uppercase `.C`/`.H` C++ source/header
        // conventions (opencode's extension list).
        let clang = find("clang-format");
        assert!(clang.extensions.iter().any(|e| e == ".C"));
        assert!(clang.extensions.iter().any(|e| e == ".H"));
    }

    #[test]
    fn which_finds_a_known_binary() {
        // `sh` exists on every unix CI runner.
        assert!(which("sh").is_some());
        assert!(which("definitely-not-a-real-binary-xyz").is_none());
    }
}
