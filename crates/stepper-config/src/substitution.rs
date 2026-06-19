//! Slash-command template substitution (Claude-Code-compatible), so authored
//! commands port directly. The engine here owns the parsing and ordering; the
//! side-effecting parts (running `!`shell``, reading files, `@include`) are
//! injected via [`SubstitutionIo`] so this crate stays free of process/permission
//! concerns — `stepper-core` supplies a permission-gated implementation.

use crate::error::ConfigError;
use regex::{Captures, Regex};
use std::collections::BTreeMap;

/// The arguments a command was invoked with.
#[derive(Debug, Clone, Default)]
pub struct CommandArgs {
    pub positional: Vec<String>,
    pub named: BTreeMap<String, String>,
}

/// The IO side-effects substitution may perform. Implementations decide what is
/// allowed (e.g. a sandboxed, permission-checked shell).
pub trait SubstitutionIo {
    fn run_shell(&self, cmd: &str) -> Result<String, String>;
    fn read_file(&self, path: &str) -> Result<String, String>;
    fn read_env(&self, var: &str) -> Option<String>;
    fn include(&self, path: &str) -> Result<String, String>;
}

const ESCAPED_DOLLAR: &str = "\u{0}STEPPER_DOLLAR\u{0}";
// Injected content (shell/file/env/include output and argument values) is
// protected with these so later passes treat it as literal text — output is
// never re-scanned for substitutions (matches Claude Code).
const PH_DOLLAR: &str = "\u{0}SD\u{0}";
const PH_LBRACE: &str = "\u{0}SL\u{0}";
const PH_AT: &str = "\u{0}SA\u{0}";

/// Expand a command template. Order matches Claude Code: shell → args → files →
/// env → includes → bare `@path`. `\$` is a literal dollar, and substituted
/// output — including argument values — is not re-scanned.
pub fn substitute(
    template: &str,
    args: &CommandArgs,
    io: &dyn SubstitutionIo,
) -> Result<String, ConfigError> {
    let mut text = template.replace("\\$", ESCAPED_DOLLAR);

    text = expand_shell(&text, io)?;
    text = expand_args(&text, args);
    text = expand_files(&text, io)?;
    text = expand_env(&text, io);
    text = expand_includes(&text, io)?;
    text = expand_bare_at(&text, io)?;

    Ok(unprotect(text.replace(ESCAPED_DOLLAR, "$")))
}

/// Encode the substitution-trigger characters in injected output so subsequent
/// passes skip it.
fn protect(s: String) -> String {
    s.replace('$', PH_DOLLAR)
        .replace('{', PH_LBRACE)
        .replace('@', PH_AT)
}

fn unprotect(s: String) -> String {
    s.replace(PH_DOLLAR, "$")
        .replace(PH_LBRACE, "{")
        .replace(PH_AT, "@")
}

fn expand_shell(text: &str, io: &dyn SubstitutionIo) -> Result<String, ConfigError> {
    // Fenced ```! blocks first, then inline !`cmd` (must start a line or follow
    // whitespace).
    let fenced = Regex::new(r"(?ms)^```!\s*\n(.*?)\n```\s*$").unwrap();
    let text = replace_fallible(&fenced, text, |c| {
        io.run_shell(c[1].trim())
            .map(protect)
            .map_err(ConfigError::Substitution)
    })?;

    let inline = Regex::new(r"(^|\s)!`([^`]+)`").unwrap();
    replace_fallible(&inline, &text, |c| {
        let out = io.run_shell(c[2].trim()).map_err(ConfigError::Substitution)?;
        Ok(format!("{}{}", &c[1], protect(out)))
    })
}

fn expand_args(text: &str, args: &CommandArgs) -> String {
    // Argument values are user input, not template: protect() them so the
    // later file/env/include/@ passes never interpret markers smuggled in an
    // argument (`/cmd '{file:/etc/shadow}'` stays literal).
    let indexed = Regex::new(r"\$ARGUMENTS\[(\d+)\]").unwrap();
    let text = indexed.replace_all(text, |c: &Captures| {
        let i: usize = c[1].parse().unwrap_or(usize::MAX);
        protect(args.positional.get(i).cloned().unwrap_or_default())
    });

    // Word-boundary match so `$ARGUMENTS` does not also consume the prefix of a
    // named arg like `$ARGUMENTS_EXTRA` (which `str::replace` would mangle).
    let arguments = Regex::new(r"\$ARGUMENTS\b").unwrap();
    let joined = protect(args.positional.join(" "));
    let all = arguments.replace_all(&text, |_: &Captures| joined.clone());

    let positional = Regex::new(r"\$(\d+)").unwrap();
    let text = positional.replace_all(&all, |c: &Captures| {
        let n: usize = c[1].parse().unwrap_or(0);
        protect(
            n.checked_sub(1)
                .and_then(|i| args.positional.get(i))
                .cloned()
                .unwrap_or_default(),
        )
    });

    let named_dollar = Regex::new(r"\$([A-Za-z_][A-Za-z0-9_]*)").unwrap();
    let text = named_dollar.replace_all(&text, |c: &Captures| {
        // An unknown `$WORD` is left literal (not deleted) — it is almost always a
        // shell variable like `$PATH`/`$HOME` the command means to expand itself.
        match args.named.get(&c[1]) {
            Some(v) => protect(v.clone()),
            None => c[0].to_string(),
        }
    });

    // {arg:name} and bare {name} (colon-free, so {file:..}/{env:..} are skipped).
    let braced_arg = Regex::new(r"\{arg:([A-Za-z0-9_-]+)\}").unwrap();
    let text = braced_arg.replace_all(&text, |c: &Captures| {
        protect(args.named.get(&c[1]).cloned().unwrap_or_default())
    });
    let braced = Regex::new(r"\{([A-Za-z0-9_-]+)\}").unwrap();
    braced
        .replace_all(&text, |c: &Captures| {
            protect(args.named.get(&c[1]).cloned().unwrap_or_default())
        })
        .into_owned()
}

fn expand_files(text: &str, io: &dyn SubstitutionIo) -> Result<String, ConfigError> {
    let re = Regex::new(r"@?\{file:([^}]+)\}").unwrap();
    replace_fallible(&re, text, |c| {
        io.read_file(c[1].trim())
            .map(protect)
            .map_err(ConfigError::Substitution)
    })
}

fn expand_env(text: &str, io: &dyn SubstitutionIo) -> String {
    let re = Regex::new(r"\{env:([A-Za-z_][A-Za-z0-9_]*)\}").unwrap();
    re.replace_all(text, |c: &Captures| {
        protect(io.read_env(&c[1]).unwrap_or_default())
    })
    .into_owned()
}

fn expand_includes(text: &str, io: &dyn SubstitutionIo) -> Result<String, ConfigError> {
    let re = Regex::new(r"@include\s+(\S+)").unwrap();
    replace_fallible(&re, text, |c| {
        io.include(&c[1]).map(protect).map_err(ConfigError::Substitution)
    })
}

fn expand_bare_at(text: &str, io: &dyn SubstitutionIo) -> Result<String, ConfigError> {
    // Bare @path: not @{...} (already handled) and not whitespace.
    let re = Regex::new(r"(^|\s)@([^\s{][^\s]*)").unwrap();
    replace_fallible(&re, text, |c| {
        let body = io.read_file(&c[2]).map_err(ConfigError::Substitution)?;
        Ok(format!("{}{}", &c[1], protect(body)))
    })
}

fn replace_fallible(
    re: &Regex,
    text: &str,
    mut f: impl FnMut(&Captures) -> Result<String, ConfigError>,
) -> Result<String, ConfigError> {
    let mut error = None;
    let out = re
        .replace_all(text, |c: &Captures| match f(c) {
            Ok(s) => s,
            Err(e) => {
                if error.is_none() {
                    error = Some(e);
                }
                String::new()
            }
        })
        .into_owned();
    match error {
        Some(e) => Err(e),
        None => Ok(out),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeIo;
    impl SubstitutionIo for FakeIo {
        fn run_shell(&self, cmd: &str) -> Result<String, String> {
            Ok(format!("<out:{cmd}>"))
        }
        fn read_file(&self, path: &str) -> Result<String, String> {
            Ok(format!("<file:{path}>"))
        }
        fn read_env(&self, var: &str) -> Option<String> {
            (var == "HOME").then(|| "/home/me".to_string())
        }
        fn include(&self, path: &str) -> Result<String, String> {
            Ok(format!("<inc:{path}>"))
        }
    }

    fn args() -> CommandArgs {
        let mut named = BTreeMap::new();
        named.insert("path".to_string(), "src/lib.rs".to_string());
        CommandArgs {
            positional: vec!["one".into(), "two".into()],
            named,
        }
    }

    #[test]
    fn expands_all_forms_in_order() {
        let tpl = "Diff: !`git diff`\nAll: $ARGUMENTS\nFirst: $1\nNamed: {arg:path}\nFile: {file:a.rs}\nEnv: {env:HOME}\nInc: @include partials/x.md\nAt: @b.rs";
        let out = substitute(tpl, &args(), &FakeIo).unwrap();
        assert!(out.contains("Diff: <out:git diff>"));
        assert!(out.contains("All: one two"));
        assert!(out.contains("First: one"));
        assert!(out.contains("Named: src/lib.rs"));
        assert!(out.contains("File: <file:a.rs>"));
        assert!(out.contains("Env: /home/me"));
        assert!(out.contains("Inc: <inc:partials/x.md>"));
        assert!(out.contains("At: <file:b.rs>"));
    }

    #[test]
    fn shell_output_is_not_rescanned() {
        struct Echo;
        impl SubstitutionIo for Echo {
            fn run_shell(&self, _: &str) -> Result<String, String> {
                Ok("$1 and {arg:x} and @file".into())
            }
            fn read_file(&self, _: &str) -> Result<String, String> {
                Ok("SHOULD-NOT-APPEAR".into())
            }
            fn read_env(&self, _: &str) -> Option<String> {
                None
            }
            fn include(&self, _: &str) -> Result<String, String> {
                Ok(String::new())
            }
        }
        let out = substitute("!`whatever`", &args(), &Echo).unwrap();
        assert_eq!(out, "$1 and {arg:x} and @file");
    }

    #[test]
    fn escaped_dollar_is_literal() {
        let out = substitute("price is \\$5 not $1", &args(), &FakeIo).unwrap();
        assert_eq!(out, "price is $5 not one");
    }

    #[test]
    fn missing_named_arg_becomes_empty() {
        let out = substitute("x={arg:missing}y", &CommandArgs::default(), &FakeIo).unwrap();
        assert_eq!(out, "x=y");
    }

    #[test]
    fn shell_failure_propagates() {
        struct FailIo;
        impl SubstitutionIo for FailIo {
            fn run_shell(&self, _: &str) -> Result<String, String> {
                Err("denied".into())
            }
            fn read_file(&self, _: &str) -> Result<String, String> {
                Ok(String::new())
            }
            fn read_env(&self, _: &str) -> Option<String> {
                None
            }
            fn include(&self, _: &str) -> Result<String, String> {
                Ok(String::new())
            }
        }
        assert!(substitute("!`rm -rf /`", &CommandArgs::default(), &FailIo).is_err());
    }

    #[test]
    fn fenced_shell_block_is_expanded() {
        let tpl = "before\n```!\nls -la\n```\nafter";
        let out = substitute(tpl, &CommandArgs::default(), &FakeIo).unwrap();
        assert!(out.contains("<out:ls -la>"));
        assert!(out.contains("before"));
        assert!(out.contains("after"));
    }

    #[test]
    fn indexed_arguments_select_by_position() {
        let out = substitute("a=$ARGUMENTS[0] b=$ARGUMENTS[1]", &args(), &FakeIo).unwrap();
        assert_eq!(out, "a=one b=two");
    }

    #[test]
    fn out_of_range_indexed_argument_is_empty() {
        let out = substitute("x=$ARGUMENTS[9]y", &args(), &FakeIo).unwrap();
        assert_eq!(out, "x=y");
    }

    #[test]
    fn positional_dollar_numbers_are_one_based() {
        let out = substitute("$1-$2-$3", &args(), &FakeIo).unwrap();
        assert_eq!(out, "one-two-");
    }

    #[test]
    fn named_dollar_variable_is_substituted() {
        let mut named = BTreeMap::new();
        named.insert("BRANCH".to_string(), "main".to_string());
        let args = CommandArgs {
            positional: vec![],
            named,
        };
        let out = substitute("on $BRANCH", &args, &FakeIo).unwrap();
        assert_eq!(out, "on main");
    }

    #[test]
    fn arguments_does_not_consume_a_named_arg_prefix() {
        let mut named = BTreeMap::new();
        named.insert("ARGUMENTS_EXTRA".to_string(), "X".to_string());
        let args = CommandArgs {
            positional: vec!["a".to_string(), "b".to_string()],
            named,
        };
        let out = substitute("$ARGUMENTS and $ARGUMENTS_EXTRA", &args, &FakeIo).unwrap();
        assert_eq!(out, "a b and X");
    }

    #[test]
    fn unknown_dollar_word_is_left_literal_not_deleted() {
        // A `$WORD` with no matching named arg stays literal (it is almost always
        // a shell variable the command expects to expand itself), never deleted.
        let out = substitute(
            "export PATH=$PATH:/x and $HOME",
            &CommandArgs::default(),
            &FakeIo,
        )
        .unwrap();
        assert_eq!(out, "export PATH=$PATH:/x and $HOME");
    }

    #[test]
    fn file_output_is_not_rescanned() {
        struct FileIo;
        impl SubstitutionIo for FileIo {
            fn run_shell(&self, _: &str) -> Result<String, String> {
                Ok(String::new())
            }
            fn read_file(&self, _: &str) -> Result<String, String> {
                Ok("$1 {arg:path} @other.rs".into())
            }
            fn read_env(&self, _: &str) -> Option<String> {
                None
            }
            fn include(&self, _: &str) -> Result<String, String> {
                Ok(String::new())
            }
        }
        let out = substitute("{file:a.rs}", &args(), &FileIo).unwrap();
        assert_eq!(out, "$1 {arg:path} @other.rs");
    }

    #[test]
    fn env_output_is_not_rescanned() {
        struct EnvIo;
        impl SubstitutionIo for EnvIo {
            fn run_shell(&self, _: &str) -> Result<String, String> {
                Ok(String::new())
            }
            fn read_file(&self, _: &str) -> Result<String, String> {
                Ok("FROM-FILE".into())
            }
            fn read_env(&self, _: &str) -> Option<String> {
                Some("@injected.rs".into())
            }
            fn include(&self, _: &str) -> Result<String, String> {
                Ok(String::new())
            }
        }
        let out = substitute("v={env:ANYTHING}", &CommandArgs::default(), &EnvIo).unwrap();
        assert_eq!(out, "v=@injected.rs");
    }

    #[test]
    fn missing_env_var_becomes_empty() {
        let out = substitute("v={env:NOPE}!", &CommandArgs::default(), &FakeIo).unwrap();
        assert_eq!(out, "v=!");
    }

    #[test]
    fn at_file_form_reads_via_read_file() {
        let out = substitute("see @{file:notes.md}", &CommandArgs::default(), &FakeIo).unwrap();
        assert_eq!(out, "see <file:notes.md>");
    }

    #[test]
    fn bare_at_only_matches_after_whitespace_or_start() {
        let out = substitute("email@example.com and @real.rs", &CommandArgs::default(), &FakeIo).unwrap();
        assert!(out.contains("email@example.com"));
        assert!(out.contains("<file:real.rs>"));
        assert!(!out.contains("<file:example.com>"));
    }

    #[test]
    fn include_failure_propagates() {
        struct IncFail;
        impl SubstitutionIo for IncFail {
            fn run_shell(&self, _: &str) -> Result<String, String> {
                Ok(String::new())
            }
            fn read_file(&self, _: &str) -> Result<String, String> {
                Ok(String::new())
            }
            fn read_env(&self, _: &str) -> Option<String> {
                None
            }
            fn include(&self, _: &str) -> Result<String, String> {
                Err("include denied".into())
            }
        }
        let err = substitute("@include x.md", &CommandArgs::default(), &IncFail).unwrap_err();
        assert!(matches!(err, ConfigError::Substitution(m) if m == "include denied"));
    }

    #[test]
    fn file_read_failure_propagates() {
        struct ReadFail;
        impl SubstitutionIo for ReadFail {
            fn run_shell(&self, _: &str) -> Result<String, String> {
                Ok(String::new())
            }
            fn read_file(&self, _: &str) -> Result<String, String> {
                Err("no such file".into())
            }
            fn read_env(&self, _: &str) -> Option<String> {
                None
            }
            fn include(&self, _: &str) -> Result<String, String> {
                Ok(String::new())
            }
        }
        assert!(substitute("{file:gone.rs}", &CommandArgs::default(), &ReadFail).is_err());
    }

    #[test]
    fn template_without_substitutions_is_unchanged() {
        let out = substitute("plain text, no markers", &CommandArgs::default(), &FakeIo).unwrap();
        assert_eq!(out, "plain text, no markers");
    }

    fn args_with_named(name: &str, value: &str) -> CommandArgs {
        let mut named = BTreeMap::new();
        named.insert(name.to_string(), value.to_string());
        CommandArgs {
            positional: vec![],
            named,
        }
    }

    #[test]
    fn arg_value_with_file_marker_stays_literal() {
        let args = args_with_named("p", "{file:gone.rs}");
        let out = substitute("{arg:p}", &args, &FakeIo).unwrap();
        assert_eq!(out, "{file:gone.rs}");
    }

    #[test]
    fn arg_value_with_env_marker_stays_literal() {
        let args = args_with_named("p", "{env:HOME}");
        let out = substitute("{arg:p}", &args, &FakeIo).unwrap();
        assert_eq!(out, "{env:HOME}");
    }

    #[test]
    fn arg_value_with_bare_at_marker_stays_literal() {
        let args = args_with_named("p", "@grabbed.rs");
        let out = substitute("{arg:p}", &args, &FakeIo).unwrap();
        assert_eq!(out, "@grabbed.rs");
    }

    #[test]
    fn positional_arg_value_with_markers_stays_literal() {
        let args = CommandArgs {
            positional: vec!["{file:/etc/shadow}".into(), "@~/.ssh/id_rsa".into()],
            named: BTreeMap::new(),
        };
        let out = substitute("a=$1 b=$2 all: $ARGUMENTS", &args, &FakeIo).unwrap();
        assert_eq!(
            out,
            "a={file:/etc/shadow} b=@~/.ssh/id_rsa all: {file:/etc/shadow} @~/.ssh/id_rsa"
        );
    }

    #[test]
    fn indexed_and_named_dollar_arg_values_with_markers_stay_literal() {
        let mut named = BTreeMap::new();
        named.insert("KEY".to_string(), "{env:HOME}".to_string());
        let args = CommandArgs {
            positional: vec!["{env:HOME}".into()],
            named,
        };
        let out = substitute("i=$ARGUMENTS[0] n=$KEY", &args, &FakeIo).unwrap();
        assert_eq!(out, "i={env:HOME} n={env:HOME}");
    }

    #[test]
    fn arg_value_with_shell_marker_is_not_executed() {
        let args = args_with_named("p", "!`rm -rf /`");
        let out = substitute("{arg:p}", &args, &FakeIo).unwrap();
        assert_eq!(out, "!`rm -rf /`");
    }

    #[test]
    fn inline_shell_mid_token_is_not_executed_but_line_start_is() {
        let tpl = "x!`echo no`\n!`echo yes`";
        let out = substitute(tpl, &CommandArgs::default(), &FakeIo).unwrap();
        assert!(out.contains("x!`echo no`"));
        assert!(!out.contains("<out:echo no>"));
        assert!(out.contains("<out:echo yes>"));
    }

    #[test]
    fn inline_shell_after_space_is_executed() {
        let out = substitute("run !`echo hi`", &CommandArgs::default(), &FakeIo).unwrap();
        assert_eq!(out, "run <out:echo hi>");
    }
}
