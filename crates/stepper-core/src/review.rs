//! `/code-review` — turn a git diff into a single-pass review prompt.
//!
//! The built-in gathers the diff core-side (it is an explicit user command, so
//! no permission round-trip is needed for the read-only git calls) and hands
//! the agent one prompt through the same expanded-command turn path that
//! `.stepper/commands` files use. Targets: no argument = uncommitted changes,
//! falling back to the branch diff against the default branch; `<ref>` /
//! `<a>..<b>` = that git range; `123` / `#123` = a GitHub PR via `gh pr diff`.
//! `--fix` appends an apply-the-fixes stage after the report.

use std::path::Path;
use std::process::Command;

/// Diffs larger than this are not embedded verbatim; the prompt falls back to
/// `--stat` plus instructions to read hunks with git so the review turn does
/// not blow the context window before it starts.
const MAX_EMBED_BYTES: usize = 96_000;

/// Bases tried (in order) for the branch-diff fallback when the working tree
/// is clean and no explicit target was given.
const BASE_CANDIDATES: &[&str] = &["origin/main", "origin/master", "main", "master"];

/// Build the `/code-review` prompt, or a user-facing refusal (`Err`) when there
/// is nothing to review / git is unavailable. Blocking (runs git), so call it
/// from `spawn_blocking`.
pub fn code_review_prompt(args: &str, cwd: &Path) -> Result<String, String> {
    let mut fix = false;
    let mut target: Option<&str> = None;
    for tok in args.split_whitespace() {
        match tok {
            "--fix" => fix = true,
            _ if target.is_none() => target = Some(tok),
            _ => {
                return Err(format!(
                    "unexpected argument '{tok}' — usage: /code-review [<ref>|<a>..<b>|#<pr>] [--fix]"
                ));
            }
        }
    }

    let (label, diff) = match target {
        Some(t) if is_pr_number(t) => {
            let n = t.trim_start_matches('#');
            let diff = run(cwd, "gh", &["pr", "diff", n])
                .map_err(|e| format!("could not fetch PR #{n} via `gh pr diff`: {e}"))?;
            if diff.trim().is_empty() {
                return Err(format!("PR #{n} has an empty diff"));
            }
            (format!("GitHub PR #{n}"), diff)
        }
        Some(t) => {
            let diff = run(cwd, "git", &["diff", t])
                .map_err(|e| format!("`git diff {t}` failed: {e}"))?;
            if diff.trim().is_empty() {
                return Err(format!("`git diff {t}` is empty — nothing to review"));
            }
            (format!("`git diff {t}`"), diff)
        }
        None => default_target(cwd)?,
    };

    Ok(compose(&label, &diff, fix))
}

/// No explicit target: uncommitted changes first, then the branch diff against
/// the first base candidate that exists and actually differs from HEAD.
fn default_target(cwd: &Path) -> Result<(String, String), String> {
    run(cwd, "git", &["rev-parse", "--is-inside-work-tree"])
        .map_err(|_| "not inside a git repository".to_string())?;
    let worktree = run(cwd, "git", &["diff", "HEAD"])
        .map_err(|e| format!("`git diff HEAD` failed (no commits yet?): {e}"))?;
    if !worktree.trim().is_empty() {
        return Ok(("uncommitted changes (`git diff HEAD`)".to_string(), worktree));
    }
    for base in BASE_CANDIDATES {
        if run(cwd, "git", &["rev-parse", "--verify", "--quiet", base]).is_err() {
            continue;
        }
        if let Ok(diff) = run(cwd, "git", &["diff", &format!("{base}...HEAD")])
            && !diff.trim().is_empty()
        {
            return Ok((format!("branch changes (`git diff {base}...HEAD`)"), diff));
        }
    }
    Err(format!(
        "nothing to review: the working tree is clean and there is no branch diff against {}",
        BASE_CANDIDATES.join(" / ")
    ))
}

/// `123` or `#123` — a GitHub PR number for `gh pr diff`.
fn is_pr_number(tok: &str) -> bool {
    let digits = tok.strip_prefix('#').unwrap_or(tok);
    !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
}

fn run(cwd: &Path, program: &str, args: &[&str]) -> Result<String, String> {
    let output = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(stderr.trim().to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn compose(label: &str, diff: &str, fix: bool) -> String {
    let mut prompt = format!(
        "Perform a focused code review of {label}.\n\n\
         Review priorities, in order:\n\
         1. Correctness bugs — a concrete input/state that produces a wrong result, crash, hang, or data loss.\n\
         2. Security issues — injection, path escape, secret exposure, missing validation at a trust boundary.\n\
         3. Simplification / reuse — dead code, duplicated logic, an existing helper that should be used.\n\
         4. Missing test coverage for the changed behavior.\n\n\
         Before reporting a finding, verify it against the surrounding code (read the touched files with \
         your tools — the diff alone lacks context). Drop anything you cannot back with a concrete failure \
         scenario. Do not report style or formatting nits.\n\n\
         Output: findings ranked most-severe first, each with `file:line`, a one-sentence defect statement, \
         and the failure scenario. If nothing survives verification, say the diff looks correct and why.\n"
    );
    if fix {
        prompt.push_str(
            "\nAfter reporting, apply the confirmed fixes directly with minimal edits (leave anything \
             uncertain as a report-only finding), then summarize what was changed.\n",
        );
    }
    if diff.len() > MAX_EMBED_BYTES {
        let stat_tail = summarize_oversized(diff);
        prompt.push_str(&format!(
            "\nThe diff is too large to embed ({} bytes). Changed files:\n\n{stat_tail}\n\
             Read the hunks yourself (e.g. `git diff -- <file>`) and review file by file.\n",
            diff.len()
        ));
    } else {
        prompt.push_str(&format!("\n````diff\n{diff}\n````\n"));
    }
    prompt
}

/// A `--stat`-like file list derived from the oversized diff itself, so the
/// fallback needs no second git invocation (and works for `gh pr diff` too).
fn summarize_oversized(diff: &str) -> String {
    let files: Vec<&str> = diff
        .lines()
        .filter_map(|l| l.strip_prefix("+++ b/"))
        .collect();
    if files.is_empty() {
        "(file list unavailable — run `git diff --stat`)".to_string()
    } else {
        files.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .expect("git runs");
        assert!(status.status.success(), "git {args:?} failed");
    }

    fn init_repo(dir: &Path) {
        git(dir, &["init", "-q", "-b", "main"]);
        fs::write(dir.join("a.txt"), "one\n").unwrap();
        git(dir, &["add", "a.txt"]);
        git(dir, &["commit", "-qm", "init"]);
    }

    #[test]
    fn dirty_worktree_diff_is_embedded_with_the_uncommitted_label() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        fs::write(tmp.path().join("a.txt"), "one\ntwo\n").unwrap();
        let prompt = code_review_prompt("", tmp.path()).unwrap();
        assert!(prompt.contains("uncommitted changes"));
        assert!(prompt.contains("+two"));
    }

    #[test]
    fn clean_tree_falls_back_to_the_branch_diff_against_main() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        git(tmp.path(), &["checkout", "-qb", "feat"]);
        fs::write(tmp.path().join("a.txt"), "one\nfeat\n").unwrap();
        git(tmp.path(), &["commit", "-qam", "feat"]);
        let prompt = code_review_prompt("", tmp.path()).unwrap();
        assert!(prompt.contains("main...HEAD"));
        assert!(prompt.contains("+feat"));
    }

    #[test]
    fn clean_tree_without_a_branch_diff_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        let err = code_review_prompt("", tmp.path()).unwrap_err();
        assert!(err.contains("nothing to review"));
    }

    #[test]
    fn outside_a_repo_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(code_review_prompt("", tmp.path()).is_err());
    }

    #[test]
    fn fix_flag_appends_the_apply_stage_and_extra_args_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        fs::write(tmp.path().join("a.txt"), "one\ntwo\n").unwrap();
        let prompt = code_review_prompt("--fix", tmp.path()).unwrap();
        assert!(prompt.contains("apply the confirmed fixes"));
        let err = code_review_prompt("x y", tmp.path()).unwrap_err();
        assert!(err.contains("unexpected argument"));
    }

    #[test]
    fn explicit_ref_target_diffs_that_range() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        fs::write(tmp.path().join("a.txt"), "one\ntwo\n").unwrap();
        git(tmp.path(), &["commit", "-qam", "second"]);
        let prompt = code_review_prompt("HEAD~1..HEAD", tmp.path()).unwrap();
        assert!(prompt.contains("`git diff HEAD~1..HEAD`"));
        assert!(prompt.contains("+two"));
    }

    #[test]
    fn oversized_diff_falls_back_to_a_file_list() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        let big = "x\n".repeat(MAX_EMBED_BYTES);
        fs::write(tmp.path().join("big.txt"), big).unwrap();
        git(tmp.path(), &["add", "big.txt"]);
        let prompt = code_review_prompt("", tmp.path()).unwrap();
        assert!(prompt.contains("too large to embed"));
        assert!(prompt.contains("big.txt"));
        assert!(!prompt.contains("````diff"));
    }
}
