//! The `apply_patch` tool: a stripped-down, file-oriented diff format that edits,
//! creates, deletes, and moves files in one structured call. The patch language
//! mirrors the OpenAI/opencode `apply_patch` envelope so models already trained on
//! it format correctly:
//!
//! ```text
//! *** Begin Patch
//! *** Add File: path        (every following line is a `+` line)
//! *** Update File: path     (optional `*** Move to: newpath`, then `@@` chunks)
//! *** Delete File: path
//! *** End Patch
//! ```
//!
//! Update chunks locate their context with progressively looser matching
//! (exact → trailing-ws → trim → unicode-normalized) so a near-miss in quotes,
//! dashes, or whitespace still applies — a faithful port of opencode's
//! `packages/core/src/patch.ts`. Changes are computed and gated up front, so a
//! patch is all-or-nothing on both verification and permission (nothing is written
//! until every file passes the secret/permission gate).

use crate::context::{Approval, ToolCx};
use crate::secret::is_secret_path_resolved;
use crate::tools::parse_args;
use crate::Tool;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use similar::TextDiff;
use std::path::{Path, PathBuf};
use stepper_permission::PermissionRequest;
use stepper_provider::{ToolError, ToolResult, ToolSpec};

const DESCRIPTION: &str = "Edit, create, delete, or move files with a single structured patch. \
The patch is a stripped-down diff envelope:\n\
\n\
*** Begin Patch\n\
[ one or more file sections ]\n\
*** End Patch\n\
\n\
Each file section starts with one of three headers:\n\
  *** Add File: <path>    - create a new file; every following line is a `+` line (the contents).\n\
  *** Delete File: <path> - remove an existing file; nothing follows.\n\
  *** Update File: <path> - patch an existing file in place. May be followed by `*** Move to: <path>` to rename it.\n\
\n\
An Update section contains one or more `@@` chunks. Inside a chunk, prefix lines with a single space for context, `-` to remove a line, and `+` to add a line. Optionally start a chunk with `@@ <context>` to anchor it, and end with `*** End of File` for an end-anchored match.\n\
\n\
Example:\n\
*** Begin Patch\n\
*** Add File: hello.txt\n\
+Hello world\n\
*** Update File: src/app.rs\n\
*** Move to: src/main.rs\n\
@@ fn greet()\n\
-    println!(\"Hi\");\n\
+    println!(\"Hello, world!\");\n\
*** Delete File: obsolete.txt\n\
*** End Patch\n\
\n\
Always include a header for each file and prefix added lines with `+`.";

pub struct ApplyPatch {
    spec: ToolSpec,
}

#[derive(Deserialize)]
struct ApplyPatchArgs {
    patch: String,
}

impl Default for ApplyPatch {
    fn default() -> Self {
        ApplyPatch {
            spec: ToolSpec {
                name: "apply_patch".into(),
                description: DESCRIPTION.into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "patch": {
                            "type": "string",
                            "description": "The full patch text, from `*** Begin Patch` to `*** End Patch`."
                        }
                    },
                    "required": ["patch"]
                }),
                read_only: false,
                parallel_safe: false,
            },
        }
    }
}

#[async_trait]
impl Tool for ApplyPatch {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn call(&self, args: Value, cx: &ToolCx) -> Result<ToolResult, ToolError> {
        let a: ApplyPatchArgs = parse_args(args)?;
        let hunks = parse_patch(&a.patch)
            .map_err(|e| ToolError::InvalidArgs(format!("apply_patch parse failed: {e}")))?;
        if hunks.is_empty() {
            return Err(ToolError::InvalidArgs(
                "apply_patch: empty patch (no file operations)".into(),
            ));
        }

        // Phase 1: compute every change from disk. Any failure here aborts before a
        // single byte is written.
        let mut changes = Vec::new();
        for hunk in &hunks {
            changes.push(compute_change(cx, hunk).await?);
        }

        // Phase 2: gate every change (fail-closed) before applying anything, so a
        // patch is all-or-nothing on permission too.
        for c in &changes {
            let target = c.move_to.clone().unwrap_or_else(|| c.path.clone());
            let request = match c.op {
                Op::Add => PermissionRequest::Write(target.clone()),
                Op::Update | Op::Move => PermissionRequest::Edit(target.clone()),
                Op::Delete => PermissionRequest::Write(c.path.clone()),
            };
            cx.gate(
                request,
                Approval::FileEdit {
                    path: target,
                    old: c.old.clone(),
                    new: c.new.clone(),
                },
            )
            .await?;
            // A move both writes the destination (gated above) and removes the
            // original — gate that removal too.
            if matches!(c.op, Op::Move) {
                cx.gate(
                    PermissionRequest::Write(c.path.clone()),
                    Approval::FileEdit {
                        path: c.path.clone(),
                        old: c.old.clone(),
                        new: String::new(),
                    },
                )
                .await?;
            }
        }

        // Phase 3: apply.
        let mut summary = Vec::new();
        let mut diffs = String::new();
        for c in &changes {
            match c.op {
                Op::Add => {
                    write_with_dirs(&c.path, &c.new).await?;
                    summary.push(format!("A {}", display_rel(cx, &c.path)));
                }
                Op::Update => {
                    write_with_dirs(&c.path, &c.new).await?;
                    summary.push(format!("M {}", display_rel(cx, &c.path)));
                }
                Op::Move => {
                    let dest = c.move_to.as_ref().expect("move has a destination");
                    write_with_dirs(dest, &c.new).await?;
                    if dest != &c.path {
                        tokio::fs::remove_file(&c.path).await.map_err(|e| {
                            ToolError::Execution(format!(
                                "apply_patch: moved content to {} but could not remove {}: {e}",
                                dest.display(),
                                c.path.display()
                            ))
                        })?;
                    }
                    summary.push(format!(
                        "M {} (moved from {})",
                        display_rel(cx, dest),
                        display_rel(cx, &c.path)
                    ));
                }
                Op::Delete => {
                    tokio::fs::remove_file(&c.path).await.map_err(|e| {
                        ToolError::Execution(format!(
                            "apply_patch: cannot delete {}: {e}",
                            c.path.display()
                        ))
                    })?;
                    summary.push(format!("D {}", display_rel(cx, &c.path)));
                }
            }
            if !matches!(c.op, Op::Delete) {
                let target = c.move_to.as_ref().unwrap_or(&c.path);
                let diff = TextDiff::from_lines(&c.old, &c.new)
                    .unified_diff()
                    .header("before", "after")
                    .to_string();
                if !diff.is_empty() {
                    diffs.push_str(&format!("\n--- {}\n{diff}", display_rel(cx, target)));
                }
            }
        }
        Ok(ToolResult::text(format!(
            "applied patch:\n{}\n{diffs}",
            summary.join("\n")
        )))
    }
}

/// The (relative) destination paths a patch creates or modifies — i.e. files that
/// exist after applying, excluding deletions; for a move, the destination. Used by
/// the agent loop to run format-on-edit on what `apply_patch` just wrote. Returns
/// empty if the patch doesn't parse.
pub fn patched_paths(patch: &str) -> Vec<String> {
    let Ok(hunks) = parse_patch(patch) else {
        return Vec::new();
    };
    hunks
        .into_iter()
        .filter_map(|h| match h {
            Hunk::Add { path, .. } => Some(path),
            Hunk::Update {
                path, move_path, ..
            } => Some(move_path.unwrap_or(path)),
            Hunk::Delete { .. } => None,
        })
        .collect()
}

#[derive(Clone, Copy, PartialEq)]
enum Op {
    Add,
    Update,
    Move,
    Delete,
}

struct Change {
    path: PathBuf,
    move_to: Option<PathBuf>,
    op: Op,
    old: String,
    new: String,
}

async fn compute_change(cx: &ToolCx, hunk: &Hunk) -> Result<Change, ToolError> {
    match hunk {
        Hunk::Add { path, contents } => {
            let path = cx.resolve(path);
            secret_check(&path, "create")?;
            // `Add File` creates a *new* file. Refuse to clobber an existing one
            // (which would silently destroy its contents — the approval diff shows
            // `old: ""`, hiding the loss, and auto-approve modes never even prompt).
            // The model should use `Update File` to change an existing file.
            if tokio::fs::try_exists(&path).await.unwrap_or(false) {
                return Err(ToolError::InvalidArgs(format!(
                    "apply_patch: {} already exists — use `Update File` to change it, not `Add File`",
                    path.display()
                )));
            }
            Ok(Change {
                path,
                move_to: None,
                op: Op::Add,
                old: String::new(),
                new: ensure_trailing_newline(contents),
            })
        }
        Hunk::Delete { path } => {
            let path = cx.resolve(path);
            secret_check(&path, "delete")?;
            let old = tokio::fs::read_to_string(&path).await.map_err(|e| {
                ToolError::Execution(format!(
                    "apply_patch: cannot read {} to delete: {e}",
                    path.display()
                ))
            })?;
            Ok(Change {
                path,
                move_to: None,
                op: Op::Delete,
                old,
                new: String::new(),
            })
        }
        Hunk::Update {
            path,
            move_path,
            chunks,
        } => {
            let p = cx.resolve(path);
            secret_check(&p, "update")?;
            let is_file = tokio::fs::metadata(&p)
                .await
                .map(|m| m.is_file())
                .unwrap_or(false);
            if !is_file {
                return Err(ToolError::Execution(format!(
                    "apply_patch: cannot update {} (missing or not a file)",
                    p.display()
                )));
            }
            let old = tokio::fs::read_to_string(&p).await.map_err(|e| {
                ToolError::Execution(format!("apply_patch: cannot read {}: {e}", p.display()))
            })?;
            let new = derive(path, chunks, &old)
                .map_err(|e| ToolError::Execution(format!("apply_patch: {e}")))?;
            let move_to = match move_path {
                Some(mp) => {
                    let mq = cx.resolve(mp);
                    secret_check(&mq, "move")?;
                    Some(mq)
                }
                None => None,
            };
            let op = if move_to.is_some() { Op::Move } else { Op::Update };
            Ok(Change {
                path: p,
                move_to,
                op,
                old,
                new,
            })
        }
    }
}

fn secret_check(path: &Path, verb: &str) -> Result<(), ToolError> {
    if is_secret_path_resolved(path) {
        return Err(ToolError::Denied(format!(
            "refusing to {verb} secret file {}",
            path.display()
        )));
    }
    Ok(())
}

async fn write_with_dirs(path: &Path, content: &str) -> Result<(), ToolError> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| ToolError::Execution(format!("mkdir {}: {e}", parent.display())))?;
    }
    tokio::fs::write(path, content)
        .await
        .map_err(|e| ToolError::Execution(format!("write {}: {e}", path.display())))
}

fn display_rel(cx: &ToolCx, p: &Path) -> String {
    p.strip_prefix(&cx.project_root)
        .unwrap_or(p)
        .display()
        .to_string()
}

fn ensure_trailing_newline(contents: &str) -> String {
    if contents.is_empty() || contents.ends_with('\n') {
        contents.to_string()
    } else {
        format!("{contents}\n")
    }
}

// ---------------------------------------------------------------------------
// Patch parsing + application (port of opencode `packages/core/src/patch.ts`).
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Hunk {
    Add {
        path: String,
        contents: String,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        move_path: Option<String>,
        chunks: Vec<UpdateChunk>,
    },
}

#[derive(Debug, Clone, PartialEq, Default)]
struct UpdateChunk {
    old_lines: Vec<String>,
    new_lines: Vec<String>,
    change_context: Option<String>,
    end_of_file: bool,
}

fn parse_patch(text: &str) -> Result<Vec<Hunk>, String> {
    let lines: Vec<&str> = text.trim().split('\n').collect();
    let begin = lines.iter().position(|l| l.trim() == "*** Begin Patch");
    let end = lines.iter().position(|l| l.trim() == "*** End Patch");
    let (begin, end) = match (begin, end) {
        (Some(b), Some(e)) if b < e => (b, e),
        _ => return Err("missing `*** Begin Patch` / `*** End Patch` markers".into()),
    };

    let mut hunks = Vec::new();
    let mut i = begin + 1;
    while i < end {
        let line = lines[i];
        if let Some(rest) = line.strip_prefix("*** Add File:") {
            let path = rest.trim().to_string();
            if path.is_empty() {
                return Err("invalid add-file path".into());
            }
            let (contents, next) = parse_add(&lines, i + 1)?;
            hunks.push(Hunk::Add { path, contents });
            i = next;
        } else if let Some(rest) = line.strip_prefix("*** Delete File:") {
            let path = rest.trim().to_string();
            if path.is_empty() {
                return Err("invalid delete-file path".into());
            }
            hunks.push(Hunk::Delete { path });
            i += 1;
        } else if let Some(rest) = line.strip_prefix("*** Update File:") {
            let path = rest.trim().to_string();
            if path.is_empty() {
                return Err("invalid update-file path".into());
            }
            let mut next = i + 1;
            let mut move_path = None;
            if let Some(mp) = lines.get(next).and_then(|l| l.strip_prefix("*** Move to:")) {
                let mp = mp.trim().to_string();
                if mp.is_empty() {
                    return Err("invalid move-to path".into());
                }
                move_path = Some(mp);
                next += 1;
            }
            let (chunks, n) = parse_update(&lines, next)?;
            if chunks.is_empty() {
                return Err(format!("update {path}: expected at least one @@ chunk"));
            }
            hunks.push(Hunk::Update {
                path,
                move_path,
                chunks,
            });
            i = n;
        } else {
            return Err(format!("invalid patch line: {line}"));
        }
    }
    Ok(hunks)
}

fn parse_add(lines: &[&str], start: usize) -> Result<(String, usize), String> {
    let mut content = Vec::new();
    let mut i = start;
    while i < lines.len() && !lines[i].starts_with("***") {
        let stripped = lines[i]
            .strip_prefix('+')
            .ok_or_else(|| format!("invalid add-file line (must start with `+`): {}", lines[i]))?;
        content.push(stripped.to_string());
        i += 1;
    }
    Ok((content.join("\n"), i))
}

fn parse_update(lines: &[&str], start: usize) -> Result<(Vec<UpdateChunk>, usize), String> {
    let mut chunks = Vec::new();
    let mut i = start;
    while i < lines.len() && !lines[i].starts_with("***") {
        if !lines[i].starts_with("@@") {
            return Err(format!("invalid update-file line: {}", lines[i]));
        }
        let change_context = {
            let c = lines[i][2..].trim();
            (!c.is_empty()).then(|| c.to_string())
        };
        let mut old_lines = Vec::new();
        let mut new_lines = Vec::new();
        let mut end_of_file = false;
        i += 1;
        while i < lines.len() && !lines[i].starts_with("@@") {
            let line = lines[i];
            if line == "*** End of File" {
                end_of_file = true;
                i += 1;
                break;
            }
            if line.starts_with("***") {
                break;
            }
            if let Some(s) = line.strip_prefix(' ') {
                old_lines.push(s.to_string());
                new_lines.push(s.to_string());
            } else if let Some(s) = line.strip_prefix('-') {
                old_lines.push(s.to_string());
            } else if let Some(s) = line.strip_prefix('+') {
                new_lines.push(s.to_string());
            } else {
                return Err(format!("invalid update-chunk line: {line}"));
            }
            i += 1;
        }
        chunks.push(UpdateChunk {
            old_lines,
            new_lines,
            change_context,
            end_of_file,
        });
    }
    Ok((chunks, i))
}

fn derive(path: &str, chunks: &[UpdateChunk], original: &str) -> Result<String, String> {
    let mut lines: Vec<String> = original.split('\n').map(String::from).collect();
    if lines.last().is_some_and(|s| s.is_empty()) {
        lines.pop();
    }
    let replacements = compute_replacements(&lines, path, chunks)?;
    let mut updated = lines;
    for (start, remove, insert) in replacements.into_iter().rev() {
        updated.splice(start..start + remove, insert);
    }
    if updated.last().map(String::as_str) != Some("") {
        updated.push(String::new());
    }
    Ok(updated.join("\n"))
}

fn compute_replacements(
    lines: &[String],
    path: &str,
    chunks: &[UpdateChunk],
) -> Result<Vec<(usize, usize, Vec<String>)>, String> {
    let mut replacements = Vec::new();
    let mut line_index = 0usize;
    for chunk in chunks {
        if let Some(ctx) = &chunk.change_context {
            let found = seek(lines, std::slice::from_ref(ctx), line_index, false)
                .ok_or_else(|| format!("failed to find context '{ctx}' in {path}"))?;
            line_index = found + 1;
        }
        if chunk.old_lines.is_empty() {
            replacements.push((lines.len(), 0, chunk.new_lines.clone()));
            continue;
        }
        let mut old_lines = chunk.old_lines.clone();
        let mut new_lines = chunk.new_lines.clone();
        let mut found = seek(lines, &old_lines, line_index, chunk.end_of_file);
        if found.is_none() && old_lines.last().is_some_and(String::is_empty) {
            old_lines.pop();
            if new_lines.last().is_some_and(String::is_empty) {
                new_lines.pop();
            }
            found = seek(lines, &old_lines, line_index, chunk.end_of_file);
        }
        let found = found.ok_or_else(|| {
            format!(
                "failed to find expected lines in {path}:\n{}",
                chunk.old_lines.join("\n")
            )
        })?;
        let remove = old_lines.len();
        line_index = found + remove;
        replacements.push((found, remove, new_lines));
    }
    replacements.sort_by_key(|r| r.0);
    Ok(replacements)
}

/// Locate `pattern` in `lines` at or after `start`, trying progressively looser
/// comparators. With `eof`, an end-anchored match is preferred.
fn seek(lines: &[String], pattern: &[String], start: usize, eof: bool) -> Option<usize> {
    if pattern.is_empty() || lines.len() < pattern.len() {
        return None;
    }
    let comparators: [fn(&str, &str) -> bool; 4] = [cmp_exact, cmp_rstrip, cmp_trim, cmp_normalized];
    let last_offset = lines.len() - pattern.len();
    for compare in comparators {
        if eof && last_offset >= start && matches_at(lines, pattern, last_offset, compare) {
            return Some(last_offset);
        }
        for offset in start..=last_offset {
            if matches_at(lines, pattern, offset, compare) {
                return Some(offset);
            }
        }
    }
    None
}

fn matches_at(
    lines: &[String],
    pattern: &[String],
    offset: usize,
    compare: fn(&str, &str) -> bool,
) -> bool {
    pattern
        .iter()
        .enumerate()
        .all(|(i, p)| compare(lines[offset + i].as_str(), p.as_str()))
}

fn cmp_exact(l: &str, r: &str) -> bool {
    l == r
}
fn cmp_rstrip(l: &str, r: &str) -> bool {
    l.trim_end() == r.trim_end()
}
fn cmp_trim(l: &str, r: &str) -> bool {
    l.trim() == r.trim()
}
fn cmp_normalized(l: &str, r: &str) -> bool {
    normalize(l.trim()) == normalize(r.trim())
}

/// Fold typographic variants (smart quotes, dashes, ellipsis, special spaces) so a
/// context line copied through a renderer still matches the source.
fn normalize(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => out.push('\''),
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => out.push('"'),
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}' => {
                out.push('-')
            }
            '\u{2026}' => out.push_str("..."),
            '\u{00A0}' | '\u{2009}' | '\u{202F}' => out.push(' '),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_add_update_delete_and_move() {
        // Built line-by-line so the leading-space context line (" keep") survives
        // (a `\`-continued literal would strip it).
        let patch = [
            "*** Begin Patch",
            "*** Add File: a.txt",
            "+hello",
            "+world",
            "*** Update File: b.txt",
            "*** Move to: c.txt",
            "@@",
            " keep",
            "-old",
            "+new",
            "*** Delete File: d.txt",
            "*** End Patch",
        ]
        .join("\n");
        let hunks = parse_patch(&patch).unwrap();
        assert_eq!(hunks.len(), 3);
        assert_eq!(
            hunks[0],
            Hunk::Add {
                path: "a.txt".into(),
                contents: "hello\nworld".into()
            }
        );
        match &hunks[1] {
            Hunk::Update {
                path,
                move_path,
                chunks,
            } => {
                assert_eq!(path, "b.txt");
                assert_eq!(move_path.as_deref(), Some("c.txt"));
                assert_eq!(chunks.len(), 1);
                assert_eq!(chunks[0].old_lines, vec!["keep", "old"]);
                assert_eq!(chunks[0].new_lines, vec!["keep", "new"]);
            }
            other => panic!("expected update, got {other:?}"),
        }
        assert_eq!(hunks[2], Hunk::Delete { path: "d.txt".into() });
    }

    #[test]
    fn missing_markers_is_an_error() {
        assert!(parse_patch("no markers here").is_err());
        assert!(parse_patch("*** Begin Patch\n*** Add File: x\n+y").is_err());
    }

    #[test]
    fn derive_replaces_matched_lines() {
        let original = "fn main() {\n    let x = 1;\n    println!(\"{x}\");\n}\n";
        let chunks = vec![UpdateChunk {
            old_lines: vec!["    let x = 1;".into()],
            new_lines: vec!["    let x = 2;".into()],
            change_context: None,
            end_of_file: false,
        }];
        let out = derive("f.rs", &chunks, original).unwrap();
        assert_eq!(
            out,
            "fn main() {\n    let x = 2;\n    println!(\"{x}\");\n}\n"
        );
    }

    #[test]
    fn derive_uses_change_context_to_disambiguate() {
        let original = "a\nx\nb\nx\nc\n";
        // Anchor at the line after `b`, so the second `x` is the one replaced.
        let chunks = vec![UpdateChunk {
            old_lines: vec!["x".into()],
            new_lines: vec!["X".into()],
            change_context: Some("b".into()),
            end_of_file: false,
        }];
        let out = derive("f", &chunks, original).unwrap();
        assert_eq!(out, "a\nx\nb\nX\nc\n");
    }

    #[test]
    fn derive_matches_loosely_on_trailing_whitespace() {
        // The source has trailing spaces the patch omits — rstrip comparison wins.
        let original = "alpha   \nbeta\n";
        let chunks = vec![UpdateChunk {
            old_lines: vec!["alpha".into()],
            new_lines: vec!["ALPHA".into()],
            change_context: None,
            end_of_file: false,
        }];
        let out = derive("f", &chunks, original).unwrap();
        assert_eq!(out, "ALPHA\nbeta\n");
    }

    #[test]
    fn derive_appends_when_old_lines_empty() {
        let original = "one\ntwo\n";
        let chunks = vec![UpdateChunk {
            old_lines: vec![],
            new_lines: vec!["three".into()],
            change_context: None,
            end_of_file: false,
        }];
        let out = derive("f", &chunks, original).unwrap();
        assert_eq!(out, "one\ntwo\nthree\n");
    }

    #[test]
    fn derive_errors_when_context_is_missing() {
        let original = "a\nb\n";
        let chunks = vec![UpdateChunk {
            old_lines: vec!["nope".into()],
            new_lines: vec!["x".into()],
            change_context: None,
            end_of_file: false,
        }];
        assert!(derive("f", &chunks, original).is_err());
    }
}
