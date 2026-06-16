//! Best-effort OS-level sandbox for the `bash` tool — a defense-in-depth backstop
//! *under* the permission engine, never a replacement for it. When enabled
//! (`settings.sandbox.enabled`, default off) the shell and every child it spawns
//! can only write under the configured writable roots (the project root plus
//! `permissions.additionalDirectories`); a write anywhere else fails with EPERM.
//! So a permission-engine miss — an Auto-mode per-atom Allow, or an unanalyzable
//! compound that falls open — can corrupt files inside the project but cannot
//! escape it.
//!
//! macOS: re-points the command through `/usr/bin/sandbox-exec` with a generated
//! Seatbelt (SBPL) profile — the mechanism Codex/Claude Code/Cursor all use, and
//! the only one that confines a child process on Darwin. Reads stay broad (so the
//! shell, interpreters, and dynamic libraries load) and the network stays open
//! (the permission engine, not the sandbox, is the network axis); only filesystem
//! *writes* are confined.
//!
//! Any other platform — or a host without `/usr/bin/sandbox-exec` — is a
//! documented no-op: [`confine_argv`] returns the argv unchanged and the
//! permission engine remains the sole guard. A Linux backend (landlock) would
//! slot into this same seam without touching callers.

use std::path::PathBuf;

/// Rewrite an argv (e.g. `["bash", "-c", script]`) so the spawned process can
/// only write under `writable_roots`, returning the argv to actually spawn.
/// Best-effort: on an unsupported platform — or a host missing the OS sandbox
/// launcher — the argv is returned unchanged rather than failing the command.
pub fn confine_argv(argv: Vec<String>, writable_roots: &[PathBuf]) -> Vec<String> {
    #[cfg(target_os = "macos")]
    {
        if let Some(wrapped) = macos::wrap(&argv, writable_roots) {
            return wrapped;
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = writable_roots;
    }
    argv
}

#[cfg(target_os = "macos")]
mod macos {
    use std::path::{Path, PathBuf};

    /// Only ever the system `sandbox-exec`: pinning the absolute path removes the
    /// PATH-injection vector (an attacker able to shadow it on `PATH` would
    /// already have the write access we are trying to deny). Mirrors Codex.
    const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

    /// Default-deny Seatbelt base, adapted from public references (OpenAI Codex's
    /// `seatbelt_base_policy.sbpl` and Chromium's macOS sandbox profile), trimmed
    /// to what a permission-vetted shell needs to *run*: exec/fork, sysctl + user
    /// lookups, POSIX shm/sem, and ptys. Reads, writes, and network are layered on
    /// by [`build_profile`].
    const BASE_POLICY: &str = r#"(version 1)
(deny default)
(allow process-exec)
(allow process-fork)
(allow signal (target same-sandbox))
(allow process-info* (target same-sandbox))
(allow sysctl-read)
(allow file-write-data (require-all (path "/dev/null") (vnode-type CHARACTER-DEVICE)))
(allow mach-lookup
  (global-name "com.apple.system.opendirectoryd.libinfo")
  (global-name "com.apple.cfprefsd.daemon")
  (global-name "com.apple.cfprefsd.agent"))
(allow user-preference-read)
(allow ipc-posix-sem)
(allow ipc-posix-shm*)
(allow pseudo-tty)
(allow file-read* file-write* file-ioctl (literal "/dev/ptmx"))
(allow file-ioctl (regex #"^/dev/ttys[0-9]+"))
"#;

    /// Build the `sandbox-exec` argv that runs `argv` confined to `writable_roots`,
    /// or `None` when the host lacks `/usr/bin/sandbox-exec` (caller runs it
    /// unconfined).
    pub fn wrap(argv: &[String], writable_roots: &[PathBuf]) -> Option<Vec<String>> {
        if !Path::new(SANDBOX_EXEC).exists() {
            return None;
        }
        let roots = resolve_roots(writable_roots);
        let (profile, params) = build_profile(&roots);

        let mut out = Vec::with_capacity(argv.len() + params.len() + 4);
        out.push(SANDBOX_EXEC.to_string());
        out.push("-p".to_string());
        out.push(profile);
        for (key, value) in params {
            // `-DKEY=VALUE`: each path reaches the SBPL only through its
            // `(param "KEY")`, never string-interpolated, so a path containing
            // quotes or spaces cannot break out of the profile.
            out.push(format!("-D{key}={}", value.to_string_lossy()));
        }
        out.push("--".to_string());
        out.extend_from_slice(argv);
        Some(out)
    }

    /// Canonicalize + dedup the writable roots and fold in the temp dirs that
    /// build tools and shell heredocs need. Canonicalization is essential:
    /// Seatbelt matches the *resolved* path, so an unresolved symlink/firmlink
    /// root (e.g. `/tmp` → `/private/tmp`) would silently deny every legitimate
    /// write under it.
    fn resolve_roots(writable_roots: &[PathBuf]) -> Vec<PathBuf> {
        fn add(p: PathBuf, out: &mut Vec<PathBuf>) {
            let norm = p.canonicalize().unwrap_or(p);
            if norm.is_absolute() && !out.contains(&norm) {
                out.push(norm);
            }
        }
        let mut out = Vec::new();
        for root in writable_roots {
            add(root.clone(), &mut out);
        }
        add(PathBuf::from("/tmp"), &mut out);
        if let Some(tmpdir) = std::env::var_os("TMPDIR") {
            add(PathBuf::from(tmpdir), &mut out);
        }
        add(std::env::temp_dir(), &mut out);
        out
    }

    /// Base policy + broad read + open network + a `file-write*` allow scoped to
    /// each writable root. Roots are passed out-of-band as `-D` params and
    /// referenced via `(param ...)`, so the policy text never embeds a path.
    fn build_profile(roots: &[PathBuf]) -> (String, Vec<(String, PathBuf)>) {
        let mut policy = String::from(BASE_POLICY);
        policy.push_str("; reads stay broad so the shell and its tools can load\n");
        policy.push_str("(allow file-read*)\n");
        policy.push_str("; the network is the permission engine's axis, not the sandbox's\n");
        policy.push_str("(allow network-outbound)\n(allow network-inbound)\n");
        policy.push_str("(allow network-bind)\n(allow system-socket)\n");

        let mut params = Vec::with_capacity(roots.len());
        if !roots.is_empty() {
            policy.push_str("(allow file-write*\n");
            for (index, root) in roots.iter().enumerate() {
                let key = format!("WRITABLE_ROOT_{index}");
                policy.push_str(&format!("  (subpath (param \"{key}\"))\n"));
                params.push((key, root.clone()));
            }
            policy.push_str(")\n");
        }
        (policy, params)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn wrap_pins_sandbox_exec_and_appends_the_original_argv() {
            let argv = vec!["bash".to_string(), "-c".to_string(), "echo hi".to_string()];
            let Some(out) = wrap(&argv, &[PathBuf::from("/tmp")]) else {
                // No /usr/bin/sandbox-exec on this host — nothing to assert.
                return;
            };
            assert_eq!(out[0], SANDBOX_EXEC);
            assert_eq!(out[1], "-p");
            // The real command is appended verbatim after the `--` separator.
            let sep = out.iter().position(|a| a == "--").expect("`--` separator");
            assert_eq!(&out[sep + 1..], argv.as_slice());
            // Each writable root is passed as a `-D` param, not interpolated.
            assert!(out.iter().any(|a| a.starts_with("-DWRITABLE_ROOT_0=")));
        }

        #[test]
        fn build_profile_scopes_writes_to_each_root_via_param() {
            let (profile, params) =
                build_profile(&[PathBuf::from("/a"), PathBuf::from("/b")]);
            assert!(profile.contains("(deny default)"));
            assert!(profile.contains("(allow file-read*)"));
            assert!(profile.contains("(subpath (param \"WRITABLE_ROOT_0\"))"));
            assert!(profile.contains("(subpath (param \"WRITABLE_ROOT_1\"))"));
            // No raw path leaks into the policy text.
            assert!(!profile.contains("/a"));
            assert_eq!(params.len(), 2);
            assert_eq!(params[0].0, "WRITABLE_ROOT_0");
        }

        #[test]
        fn resolve_roots_canonicalizes_symlinked_tmp() {
            // `/tmp` is a symlink to `/private/tmp` on macOS; the resolved set must
            // hold the real path or Seatbelt would deny writes under it.
            let roots = resolve_roots(&[PathBuf::from("/tmp")]);
            assert!(roots.iter().all(|p| p.is_absolute()));
            assert!(roots
                .iter()
                .any(|p| p.starts_with("/private/tmp") || p.as_path() == Path::new("/tmp")));
        }
    }
}
