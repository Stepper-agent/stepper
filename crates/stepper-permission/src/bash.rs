/// The placeholder a substitution span (`$(...)`, backticks, `<(...)`, `>(...)`)
/// collapses to in the masked copy of an atom, so the redirection parser can
/// recognise a dynamic target without re-reading the inner command.
const SUBSTITUTION_MARKER: char = '\u{1A}';

/// Recursion ceiling for nested substitutions — deeper than this is treated as
/// undecomposable (fail closed) rather than risking pathological input.
const MAX_NESTING: usize = 16;

/// One fully decomposed command component.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BashAtom {
    /// The original component text (substitutions kept verbatim) — what command
    /// rules are matched against.
    pub command: String,
    /// Redirection sources (`< file`) to gate as `Read` path requests.
    pub reads: Vec<String>,
    /// Redirection targets (`>`, `>>`, `&>`) to gate as `Write` path requests.
    pub writes: Vec<String>,
    /// The atom carries a redirection whose target could not be analyzed
    /// (variable, glob, substitution, `~`) — an `Allow` must escalate to `Ask`.
    pub escalate: bool,
}

/// Decompose a (possibly compound) shell command into independently gateable
/// atoms: splits on `&& || | |& ; \n` and lone `&` (quote-aware), recursively
/// extracts `$(...)`/backtick/process substitutions as additional atoms, and
/// lifts redirection targets out as Read/Write path requests. Returns `None`
/// when the command cannot be fully decomposed (unbalanced quote/paren/backtick,
/// dangling redirection) — the engine fails closed to `Deny` on that.
pub fn decompose(command: &str) -> Option<Vec<BashAtom>> {
    let mut atoms = Vec::new();
    scan(command, &mut atoms, 0)?;
    Some(atoms)
}

fn scan(command: &str, out: &mut Vec<BashAtom>, depth: usize) -> Option<()> {
    if depth > MAX_NESTING {
        return None;
    }
    let chars: Vec<char> = command.chars().collect();
    let mut text = String::new();
    let mut masked = String::new();
    let mut quote: Option<char> = None;
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];
        if quote == Some('\'') {
            text.push(c);
            masked.push(c);
            if c == '\'' {
                quote = None;
            }
            i += 1;
            continue;
        }
        match c {
            '\\' => {
                text.push(c);
                masked.push(c);
                if let Some(&next) = chars.get(i + 1) {
                    text.push(next);
                    masked.push(next);
                    i += 2;
                } else {
                    i += 1;
                }
            }
            '\'' if quote.is_none() => {
                quote = Some('\'');
                text.push(c);
                masked.push(c);
                i += 1;
            }
            '"' => {
                quote = if quote == Some('"') { None } else { Some('"') };
                text.push(c);
                masked.push(c);
                i += 1;
            }
            // Command substitution stays live inside double quotes, so these two
            // arms come before the double-quote passthrough.
            '$' if chars.get(i + 1) == Some(&'(') => {
                let end = matching_paren(&chars, i + 2)?;
                let inner: String = chars[i + 2..end].iter().collect();
                scan(&inner, out, depth + 1)?;
                text.extend(&chars[i..=end]);
                masked.push(SUBSTITUTION_MARKER);
                i = end + 1;
            }
            '`' => {
                let end = closing_backtick(&chars, i + 1)?;
                let inner: String = chars[i + 1..end].iter().collect();
                scan(&inner, out, depth + 1)?;
                text.extend(&chars[i..=end]);
                masked.push(SUBSTITUTION_MARKER);
                i = end + 1;
            }
            _ if quote == Some('"') => {
                text.push(c);
                masked.push(c);
                i += 1;
            }
            '<' | '>' if chars.get(i + 1) == Some(&'(') => {
                let end = matching_paren(&chars, i + 2)?;
                let inner: String = chars[i + 2..end].iter().collect();
                scan(&inner, out, depth + 1)?;
                text.extend(&chars[i..=end]);
                masked.push(SUBSTITUTION_MARKER);
                i = end + 1;
            }
            '&' if chars.get(i + 1) == Some(&'&') => {
                finish_atom(&mut text, &mut masked, out)?;
                i += 2;
            }
            '|' => {
                finish_atom(&mut text, &mut masked, out)?;
                i += if matches!(chars.get(i + 1), Some(&'|') | Some(&'&')) {
                    2
                } else {
                    1
                };
            }
            ';' | '\n' => {
                finish_atom(&mut text, &mut masked, out)?;
                i += 1;
            }
            // `&>` is a redirection prefix and `>&`/`<&` are fd duplications —
            // only a bare `&` is the backgrounding separator.
            '&' if chars.get(i + 1) == Some(&'>') => {
                text.push(c);
                masked.push(c);
                i += 1;
            }
            '&' if matches!(masked.chars().last(), Some('>') | Some('<')) => {
                text.push(c);
                masked.push(c);
                i += 1;
            }
            '&' => {
                finish_atom(&mut text, &mut masked, out)?;
                i += 1;
            }
            // Subshell `( … )` and brace-group `{ …; }` delimiters split commands
            // like `;` does, so a deny such as `Bash(rm -rf *)` cannot be evaded by
            // wrapping the command in a group. `$( )`, `` ` ` ``, `<( )`, `>( )` are
            // consumed by their own arms above, so a bare `(`/`)` here is grouping.
            // Brace expansion (`{1..5}`, `a.{x,y}`) is preserved: `{` only splits
            // when followed by whitespace (group syntax) and `}` only when it
            // stands alone (preceded by whitespace).
            '(' | ')' => {
                finish_atom(&mut text, &mut masked, out)?;
                i += 1;
            }
            '{' if chars.get(i + 1).is_some_and(|c| c.is_whitespace()) => {
                finish_atom(&mut text, &mut masked, out)?;
                i += 1;
            }
            '}' if text.chars().last().is_none_or(char::is_whitespace) => {
                finish_atom(&mut text, &mut masked, out)?;
                i += 1;
            }
            _ => {
                text.push(c);
                masked.push(c);
                i += 1;
            }
        }
    }
    if quote.is_some() {
        return None;
    }
    finish_atom(&mut text, &mut masked, out)
}

/// Collapse runs of unquoted whitespace to a single space so command rules match
/// regardless of incidental spacing (`rm   -rf  x` == `rm -rf x`). Quoted and
/// escaped runs are preserved verbatim.
fn normalize_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut quote: Option<char> = None;
    let mut prev_ws = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if let Some(q) = quote {
            out.push(c);
            if c == q {
                quote = None;
            }
            prev_ws = false;
            continue;
        }
        match c {
            '\'' | '"' => {
                quote = Some(c);
                out.push(c);
                prev_ws = false;
            }
            '\\' => {
                out.push(c);
                if let Some(next) = chars.next() {
                    out.push(next);
                }
                prev_ws = false;
            }
            w if w.is_whitespace() => {
                if !prev_ws {
                    out.push(' ');
                    prev_ws = true;
                }
            }
            _ => {
                out.push(c);
                prev_ws = false;
            }
        }
    }
    out.trim().to_string()
}

fn finish_atom(text: &mut String, masked: &mut String, out: &mut Vec<BashAtom>) -> Option<()> {
    let command = normalize_ws(text);
    let masked_atom = masked.trim().to_string();
    text.clear();
    masked.clear();
    if command.is_empty() {
        return Some(());
    }
    let (reads, writes, escalate) = parse_redirections(&masked_atom)?;
    // A process wrapper (`sudo`, `timeout 5`, `env X=1`, `nice`, `nohup`, …) runs
    // the command that FOLLOWS it — so a deny rule like `Bash(rm *)` must still
    // catch `timeout 5 rm -rf x`. Gate the unwrapped inner command as an extra
    // atom (the engine takes the most-restrictive verdict across atoms), which can
    // only tighten: a denied inner command is caught, an allowed one is unaffected.
    if let Some(inner) = strip_wrappers(&command)
        && inner != command
    {
        out.push(BashAtom {
            command: inner,
            reads: Vec::new(),
            writes: Vec::new(),
            escalate: false,
        });
    }
    out.push(BashAtom {
        command,
        reads,
        writes,
        escalate,
    });
    Some(())
}

/// Peel leading process-wrapper tokens off a command so the inner command can be
/// gated on its own. Returns the inner command, or `None` if the head is not a
/// wrapper. Handles each wrapper's simple option grammar (value-taking flags and
/// the one positional `timeout`/`nice` takes); unknown shapes stop peeling.
fn strip_wrappers(command: &str) -> Option<String> {
    const WRAPPERS: &[&str] = &[
        "sudo", "env", "nice", "timeout", "nohup", "stdbuf", "setsid", "ionice", "chrt", "time",
        "xargs",
    ];
    // Flags that consume the next token as their value.
    const VALUE_FLAGS: &[&str] = &[
        "-u", "--user", "-s", "--signal", "-k", "--kill-after", "-n", "--adjustment", "-P", "-o",
    ];
    let mut tokens: Vec<&str> = command.split_whitespace().collect();
    let mut peeled = false;
    // Stop when the head is not a wrapper (or nothing is left).
    while let Some(&wrapper) = tokens.first().filter(|h| WRAPPERS.contains(h)) {
        let mut i = 1;
        // Skip options / assignments the wrapper accepts before its command.
        while i < tokens.len() {
            let t = tokens[i];
            if t.contains('=') && !t.starts_with('-') {
                i += 1; // env VAR=val
            } else if VALUE_FLAGS.contains(&t) {
                i += 2; // flag + its value
            } else if t.starts_with('-') {
                i += 1; // bare flag
            } else {
                break;
            }
        }
        // `timeout DURATION cmd` / `nice N cmd`: one bare positional before cmd.
        if matches!(wrapper, "timeout" | "nice") && i < tokens.len() {
            let t = tokens[i];
            let numeric = t
                .trim_end_matches(['s', 'm', 'h', 'd'])
                .chars()
                .all(|c| c.is_ascii_digit() || c == '.');
            if numeric && !t.is_empty() {
                i += 1;
            }
        }
        if i >= tokens.len() {
            break; // nothing left to run — not a wrapping of another command
        }
        tokens = tokens.split_off(i);
        peeled = true;
    }
    peeled.then(|| tokens.join(" "))
}

fn matching_paren(chars: &[char], start: usize) -> Option<usize> {
    let mut depth = 1usize;
    let mut quote: Option<char> = None;
    let mut i = start;
    while i < chars.len() {
        let c = chars[i];
        if let Some(q) = quote {
            if q == '"' && c == '\\' {
                i += 2;
                continue;
            }
            if c == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        match c {
            '\\' => i += 1,
            '\'' | '"' => quote = Some(c),
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn closing_backtick(chars: &[char], start: usize) -> Option<usize> {
    let mut i = start;
    while i < chars.len() {
        match chars[i] {
            '\\' => i += 2,
            '`' => return Some(i),
            _ => i += 1,
        }
    }
    None
}

fn parse_redirections(masked: &str) -> Option<(Vec<String>, Vec<String>, bool)> {
    let chars: Vec<char> = masked.chars().collect();
    let mut reads = Vec::new();
    let mut writes = Vec::new();
    let mut escalate = false;
    let mut quote: Option<char> = None;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        match c {
            '\'' | '"' => {
                quote = Some(c);
                i += 1;
            }
            '\\' => i += 2,
            '&' if chars.get(i + 1) == Some(&'>') => {
                i += 2;
                if chars.get(i) == Some(&'>') {
                    i += 1;
                }
                i = consume_target(&chars, i, &mut writes, &mut escalate)?;
            }
            '>' => {
                i += 1;
                if chars.get(i) == Some(&'>') {
                    i += 1;
                }
                if chars.get(i) == Some(&'|') {
                    i += 1;
                }
                if chars.get(i) == Some(&'&') {
                    i += 1;
                    if is_fd_dup(&chars, i) {
                        i = skip_word(&chars, i);
                        continue;
                    }
                }
                i = consume_target(&chars, i, &mut writes, &mut escalate)?;
            }
            '<' => {
                i += 1;
                if chars.get(i) == Some(&'<') {
                    // heredoc / herestring: inline data, not a file target.
                    i += 1;
                    if matches!(chars.get(i), Some(&'<') | Some(&'-')) {
                        i += 1;
                    }
                    i = skip_word(&chars, i);
                    continue;
                }
                if chars.get(i) == Some(&'&') {
                    i += 1;
                    if is_fd_dup(&chars, i) {
                        i = skip_word(&chars, i);
                        continue;
                    }
                }
                i = consume_target(&chars, i, &mut reads, &mut escalate)?;
            }
            _ => i += 1,
        }
    }
    Some((reads, writes, escalate))
}

fn skip_ws(chars: &[char], mut i: usize) -> usize {
    while i < chars.len() && chars[i].is_whitespace() {
        i += 1;
    }
    i
}

fn skip_word(chars: &[char], i: usize) -> usize {
    let mut j = skip_ws(chars, i);
    while j < chars.len()
        && !chars[j].is_whitespace()
        && !matches!(chars[j], '<' | '>' | '&' | ';' | '|')
    {
        j += 1;
    }
    j
}

/// Whether the word at `i` names a file descriptor (`2>&1`, `>&-`) rather than a
/// file — those duplicate/close fds and touch no path.
fn is_fd_dup(chars: &[char], i: usize) -> bool {
    let start = skip_ws(chars, i);
    let end = skip_word(chars, start);
    end > start && chars[start..end].iter().all(|c| c.is_ascii_digit() || *c == '-')
}

/// Read the redirection target word starting at `i`. A clean, static path is
/// pushed to `targets`; a dynamic one (variable, substitution, glob, `~`) sets
/// `escalate` instead. A missing target is undecomposable (`None`).
fn consume_target(
    chars: &[char],
    i: usize,
    targets: &mut Vec<String>,
    escalate: &mut bool,
) -> Option<usize> {
    let start = skip_ws(chars, i);
    let mut j = start;
    let mut value = String::new();
    let mut dynamic = false;
    let mut quote: Option<char> = None;
    while j < chars.len() {
        let c = chars[j];
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else {
                if q == '"' && matches!(c, '$' | '`' | SUBSTITUTION_MARKER) {
                    dynamic = true;
                }
                value.push(c);
            }
            j += 1;
            continue;
        }
        match c {
            '\'' | '"' => {
                quote = Some(c);
                j += 1;
            }
            '\\' => {
                if let Some(&next) = chars.get(j + 1) {
                    value.push(next);
                    j += 2;
                } else {
                    j += 1;
                }
            }
            c if c.is_whitespace() => break,
            '<' | '>' | '&' | ';' | '|' | '(' | ')' => break,
            '$' | '`' | SUBSTITUTION_MARKER => {
                dynamic = true;
                j += 1;
            }
            '*' | '?' | '[' | '{' => {
                dynamic = true;
                value.push(c);
                j += 1;
            }
            '~' if j == start => {
                dynamic = true;
                value.push(c);
                j += 1;
            }
            _ => {
                value.push(c);
                j += 1;
            }
        }
    }
    if quote.is_some() {
        return None;
    }
    if dynamic {
        *escalate = true;
    } else if value.is_empty() {
        return None;
    } else {
        targets.push(value);
    }
    Some(j)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commands(input: &str) -> Vec<String> {
        decompose(input)
            .expect("decomposable")
            .into_iter()
            .map(|a| a.command)
            .collect()
    }

    #[test]
    fn splits_operators_outside_quotes() {
        assert_eq!(
            commands("cargo build && rm -rf /"),
            vec!["cargo build", "rm -rf /"]
        );
        assert_eq!(commands("a | b ; c || d"), vec!["a", "b", "c", "d"]);
    }

    #[test]
    fn splits_lone_ampersand_but_not_fd_dups() {
        assert_eq!(commands("echo a & rm -rf /"), vec!["echo a", "rm -rf /"]);
        assert_eq!(commands("sleep 1 &"), vec!["sleep 1"]);
        assert_eq!(commands("cargo build 2>&1"), vec!["cargo build 2>&1"]);
    }

    #[test]
    fn keeps_operators_inside_quotes() {
        assert_eq!(
            commands("echo 'a && b' | grep x"),
            vec!["echo 'a && b'", "grep x"]
        );
        assert_eq!(commands("echo 'a & b'"), vec!["echo 'a & b'"]);
    }

    #[test]
    fn extracts_command_substitution_as_additional_atom() {
        assert_eq!(
            commands("echo $(rm -rf /)"),
            vec!["rm -rf /", "echo $(rm -rf /)"]
        );
        assert_eq!(commands("ls `pwd`"), vec!["pwd", "ls `pwd`"]);
        assert_eq!(
            commands("echo \"$(rm -rf /)\""),
            vec!["rm -rf /", "echo \"$(rm -rf /)\""]
        );
    }

    #[test]
    fn extracts_nested_substitutions() {
        assert_eq!(
            commands("echo $(cat $(gen))"),
            vec!["gen", "cat $(gen)", "echo $(cat $(gen))"]
        );
    }

    #[test]
    fn extracts_process_substitutions() {
        assert_eq!(
            commands("diff <(sort a) <(sort b)"),
            vec!["sort a", "sort b", "diff <(sort a) <(sort b)"]
        );
    }

    #[test]
    fn lifts_redirection_targets_as_reads_and_writes() {
        let atoms = decompose("sort < input.txt > output.txt").unwrap();
        assert_eq!(atoms.len(), 1);
        assert_eq!(atoms[0].reads, vec!["input.txt"]);
        assert_eq!(atoms[0].writes, vec!["output.txt"]);
        assert!(!atoms[0].escalate);

        let append = decompose("echo x >> /var/log/app.log").unwrap();
        assert_eq!(append[0].writes, vec!["/var/log/app.log"]);

        let both = decompose("cmd &> all.log").unwrap();
        assert_eq!(both[0].writes, vec!["all.log"]);
    }

    #[test]
    fn quoted_metacharacters_are_not_redirections() {
        let atoms = decompose("echo 'a > b'").unwrap();
        assert_eq!(atoms.len(), 1);
        assert!(atoms[0].reads.is_empty());
        assert!(atoms[0].writes.is_empty());
        assert!(!atoms[0].escalate);
    }

    #[test]
    fn dynamic_redirection_targets_escalate() {
        assert!(decompose("echo x > $FILE").unwrap()[0].escalate);
        assert!(decompose("echo x > ~/out").unwrap()[0].escalate);
        assert!(decompose("echo x > out-*.txt").unwrap()[0].escalate);
        assert!(decompose("cat < <(ls)").unwrap().last().unwrap().escalate);
    }

    #[test]
    fn heredocs_and_fd_dups_are_harmless() {
        let heredoc = decompose("cat <<EOF").unwrap();
        assert!(heredoc[0].reads.is_empty() && !heredoc[0].escalate);
        let herestring = decompose("cat <<< data").unwrap();
        assert!(herestring[0].reads.is_empty() && !herestring[0].escalate);
        let dup = decompose("cmd >&2").unwrap();
        assert!(dup[0].writes.is_empty() && !dup[0].escalate);
    }

    #[test]
    fn splits_subshell_and_brace_groups() {
        assert_eq!(commands("(rm -rf /)"), vec!["rm -rf /"]);
        assert_eq!(commands("{ rm -rf /; }"), vec!["rm -rf /"]);
        assert_eq!(
            commands("( cargo build && rm -rf / )"),
            vec!["cargo build", "rm -rf /"]
        );
    }

    #[test]
    fn preserves_brace_expansion() {
        assert_eq!(commands("echo {1..5}"), vec!["echo {1..5}"]);
        assert_eq!(commands("mv a.{txt,bak} dir"), vec!["mv a.{txt,bak} dir"]);
    }

    #[test]
    fn collapses_incidental_whitespace_outside_quotes() {
        assert_eq!(commands("rm   -rf   x"), vec!["rm -rf x"]);
        assert_eq!(commands("echo 'a  b'"), vec!["echo 'a  b'"]);
    }

    #[test]
    fn undecomposable_commands_return_none() {
        assert!(decompose("echo $(rm -rf /").is_none());
        assert!(decompose("ls `pwd").is_none());
        assert!(decompose("echo 'unterminated").is_none());
        assert!(decompose("echo \"unterminated").is_none());
        assert!(decompose("echo >").is_none());
    }
}
