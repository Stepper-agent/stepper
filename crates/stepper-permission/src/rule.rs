use crate::path;
use regex::Regex;
use std::path::Path;

/// A parsed permission specifier, e.g. `Bash(npm run *)`, `Read(//etc/**)`,
/// `Mcp(server, tool)`, or a bare `Read` (matches any argument).
#[derive(Debug, Clone)]
pub struct Rule {
    pub tool: String,
    pub spec: Spec,
}

#[derive(Debug, Clone)]
pub enum Spec {
    Any,
    Pattern(String),
    Mcp { server: String, tool: Option<String> },
}

/// What a rule is matched against for a single (already de-compounded) request.
pub enum MatchTarget<'a> {
    Command(&'a str),
    Path(&'a Path),
    Text(&'a str),
    Mcp { server: &'a str, tool: &'a str },
}

impl Rule {
    pub fn parse(spec: &str) -> Option<Rule> {
        let spec = spec.trim();
        let Some(open) = spec.find('(') else {
            return (!spec.is_empty()).then(|| Rule {
                tool: spec.to_string(),
                spec: Spec::Any,
            });
        };
        let tool = spec[..open].trim().to_string();
        if tool.is_empty() {
            return None;
        }
        let inner = spec[open + 1..].strip_suffix(')')?.trim();
        if tool.eq_ignore_ascii_case("mcp") {
            let mut it = inner.splitn(2, ',');
            let server = it.next()?.trim().to_string();
            let tool_name = it.next().map(str::trim).filter(|t| !t.is_empty() && *t != "*");
            return Some(Rule {
                tool,
                spec: Spec::Mcp {
                    server,
                    tool: tool_name.map(str::to_string),
                },
            });
        }
        Some(Rule {
            tool,
            spec: if inner.is_empty() {
                Spec::Any
            } else {
                Spec::Pattern(inner.to_string())
            },
        })
    }

    pub fn matches(
        &self,
        request_tool: &str,
        target: &MatchTarget,
        project_root: &Path,
        home: Option<&Path>,
    ) -> bool {
        if !self.tool.eq_ignore_ascii_case(request_tool) {
            return false;
        }
        match (&self.spec, target) {
            (Spec::Any, _) => true,
            (Spec::Pattern(p), MatchTarget::Command(cmd)) => command_glob(p, cmd),
            (Spec::Pattern(p), MatchTarget::Text(t)) => command_glob(p, t),
            (Spec::Pattern(p), MatchTarget::Path(path)) => {
                path::path_matches(p, path, project_root, home)
            }
            (
                Spec::Mcp {
                    server,
                    tool: rule_tool,
                },
                MatchTarget::Mcp {
                    server: req_server,
                    tool: req_tool,
                },
            ) => {
                glob_eq(server, req_server)
                    && rule_tool.as_deref().is_none_or(|t| glob_eq(t, req_tool))
            }
            _ => false,
        }
    }
}

/// Parse a list of specifiers, dropping any that fail to parse.
pub fn parse_all(specs: &[String]) -> Vec<Rule> {
    specs.iter().filter_map(|s| Rule::parse(s)).collect()
}

/// Parse a list of specifiers, returning the parsed rules and the specs that
/// failed to parse (so the caller can warn or refuse to start).
pub fn parse_all_checked(specs: &[String]) -> (Vec<Rule>, Vec<String>) {
    let mut rules = Vec::new();
    let mut malformed = Vec::new();
    for spec in specs {
        match Rule::parse(spec) {
            Some(rule) => rules.push(rule),
            None => malformed.push(spec.clone()),
        }
    }
    (rules, malformed)
}

/// Claude-Code command specifier matching: `*` is a wildcard and a TRAILING
/// `:*` (the arg-prefix form, e.g. `git push:*`) means "this prefix then
/// anything". A mid-pattern `:` stays literal so `curl http://host:*/path`
/// cannot collapse into `http://host*` and over-match other hosts.
fn command_glob(pattern: &str, command: &str) -> bool {
    let command = command.trim();
    match pattern.strip_suffix(":*") {
        // Arg-prefix form: the prefix exactly, or the prefix followed by
        // whitespace then anything — NOT an open-ended substring, so
        // `git push:*` matches `git push origin` but not `git pushx`.
        Some(prefix) => {
            let parts: Vec<String> = prefix.split('*').map(regex::escape).collect();
            let re = format!("^{}(\\s.*)?$", parts.join(".*"));
            Regex::new(&re).map(|r| r.is_match(command)).unwrap_or(false)
        }
        None => glob_eq(pattern, command),
    }
}

fn glob_eq(pattern: &str, value: &str) -> bool {
    let parts: Vec<String> = pattern.split('*').map(regex::escape).collect();
    let re = format!("^{}$", parts.join(".*"));
    Regex::new(&re).map(|r| r.is_match(value)).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn parses_specifiers() {
        let r = Rule::parse("Bash(npm run *)").unwrap();
        assert_eq!(r.tool, "Bash");
        assert!(matches!(r.spec, Spec::Pattern(_)));

        let m = Rule::parse("Mcp(filesystem, read_file)").unwrap();
        assert!(matches!(m.spec, Spec::Mcp { .. }));

        let any = Rule::parse("Read").unwrap();
        assert!(matches!(any.spec, Spec::Any));
    }

    #[test]
    fn command_glob_handles_wildcards_and_colon_prefix() {
        let root = PathBuf::from("/p");
        let r = Rule::parse("Bash(cargo *)").unwrap();
        assert!(r.matches("Bash", &MatchTarget::Command("cargo build"), &root, None));
        assert!(!r.matches("Bash", &MatchTarget::Command("npm test"), &root, None));

        let push = Rule::parse("Bash(git push:*)").unwrap();
        assert!(push.matches("Bash", &MatchTarget::Command("git push origin"), &root, None));
        assert!(push.matches("Bash", &MatchTarget::Command("git push"), &root, None));
        // The arg-prefix form requires a word boundary: it must not over-match an
        // adjacent token like `git pushx` / `git push-all`.
        assert!(!push.matches("Bash", &MatchTarget::Command("git pushx"), &root, None));
        assert!(!push.matches("Bash", &MatchTarget::Command("git push-all"), &root, None));
    }

    #[test]
    fn mid_pattern_colon_stays_literal() {
        let root = PathBuf::from("/p");
        let host = Rule::parse("Bash(curl http://h:*/path)").unwrap();
        assert!(host.matches(
            "Bash",
            &MatchTarget::Command("curl http://h:8080/path"),
            &root,
            None
        ));
        assert!(!host.matches(
            "Bash",
            &MatchTarget::Command("curl http://hEVIL/path"),
            &root,
            None
        ));
    }

    #[test]
    fn parse_rejects_missing_close_paren_and_empty_tool() {
        assert!(Rule::parse("Bash(rm *").is_none());
        assert!(Rule::parse("(rm *)").is_none());
        assert!(Rule::parse("").is_none());
        let nested = Rule::parse("Bash(echo (x))").unwrap();
        assert!(matches!(nested.spec, Spec::Pattern(p) if p == "echo (x)"));
    }

    #[test]
    fn parse_all_checked_returns_malformed_specs() {
        let (rules, malformed) = parse_all_checked(&[
            "Bash(cargo *)".into(),
            "Bash(rm *".into(),
            "Read".into(),
        ]);
        assert_eq!(rules.len(), 2);
        assert_eq!(malformed, vec!["Bash(rm *".to_string()]);
    }

    #[test]
    fn mcp_tool_wildcard() {
        let root = PathBuf::from("/p");
        let any_tool = Rule::parse("Mcp(filesystem)").unwrap();
        assert!(any_tool.matches(
            "Mcp",
            &MatchTarget::Mcp { server: "filesystem", tool: "read_file" },
            &root,
            None
        ));
        let specific = Rule::parse("Mcp(filesystem, read_file)").unwrap();
        assert!(!specific.matches(
            "Mcp",
            &MatchTarget::Mcp { server: "filesystem", tool: "write_file" },
            &root,
            None
        ));
    }
}
