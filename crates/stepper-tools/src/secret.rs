use std::path::Path;

/// Files whose contents are secrets and must never be read/searched by a tool,
/// regardless of permission rules. Matching is case-insensitive on both the
/// basename and the full path. Public certs (`*.crt`/`*.cer`) are deliberately
/// excluded: they are not secrets and flag far too many false positives.
pub fn is_secret_path(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let full = path.to_string_lossy().to_ascii_lowercase();

    name == ".env"
        || name.starts_with(".env.")
        || name == "id_rsa"
        || name == "id_ed25519"
        || name == "id_dsa"
        || name == ".npmrc"
        || name == ".netrc"
        || name == "credentials"
        || name == ".git-credentials"
        || name == ".pgpass"
        || name == ".dockercfg"
        || name == ".terraformrc"
        || name == ".pypirc"
        || name == ".htpasswd"
        || name.ends_with(".pem")
        || name.ends_with(".key")
        || full.contains("/.ssh/")
        || full.contains("/.aws/")
        || full.contains("/.gnupg/")
        || full.contains("/.kube/config")
        || full.contains("/.docker/config.json")
        || full.contains("/.config/gh/hosts.yml")
}

/// `is_secret_path` on both the literal path and its lenient canonicalization
/// (deepest existing ancestor resolved, the non-existent remainder re-joined),
/// so a benign-named symlink to a secret — or a not-yet-existing file under a
/// symlinked secret directory — is still refused.
pub fn is_secret_path_resolved(path: &Path) -> bool {
    is_secret_path(path)
        || is_secret_path(&stepper_permission::path::resolve_request_path(
            path,
            Path::new("/"),
        ))
}

/// Tokenize a shell command (quote-aware) and return the first path-looking
/// token that resolves to a secret file, so a caller can refuse to run it —
/// `cat ~/.ssh/id_rsa` must not slip through. `Err` = untokenizable (fail
/// closed). The single screen used by both the bash tool and the background-
/// process (`!cmd &`) path, which both run user-/model-initiated shell.
pub fn find_secret_path_in_command(
    command: &str,
    cwd: &Path,
    home: Option<&Path>,
) -> Result<Option<std::path::PathBuf>, String> {
    use std::path::PathBuf;
    let tokens = shell_words::split(command)
        .map_err(|e| format!("command cannot be tokenized for secret-path screening: {e}"))?;
    for token in tokens {
        // A glob pattern (`*.pem`, `id_*`, `[abc].key`) is an argument to a
        // command like `find -name`/`ls`, not a literal file the command reads
        // or writes — screening it blocks common, harmless dev commands. A
        // literal secret path (`~/.ssh/id_rsa`, `.env`) has no metacharacters and
        // is still caught.
        if token.contains(['*', '?', '[']) {
            continue;
        }
        let looks_like_path =
            token.contains('/') || token.starts_with('~') || is_secret_path(Path::new(&token));
        if !looks_like_path {
            continue;
        }
        let expanded = match (token.strip_prefix("~/"), home) {
            (Some(rest), Some(h)) => h.join(rest),
            _ if token == "~" && home.is_some() => home.unwrap().to_path_buf(),
            _ => PathBuf::from(&token),
        };
        let abs = if expanded.is_absolute() {
            expanded
        } else {
            cwd.join(expanded)
        };
        if is_secret_path_resolved(&abs) {
            return Ok(Some(abs));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn flags_common_secrets() {
        assert!(is_secret_path(&PathBuf::from("/p/.env")));
        assert!(is_secret_path(&PathBuf::from("/home/u/.ssh/id_rsa")));
        assert!(is_secret_path(&PathBuf::from("/p/server.pem")));
        assert!(!is_secret_path(&PathBuf::from("/p/src/main.rs")));
    }

    #[test]
    fn matching_is_case_insensitive() {
        assert!(is_secret_path(&PathBuf::from("/p/.ENV")));
        assert!(is_secret_path(&PathBuf::from("/p/.Env.Local")));
        assert!(is_secret_path(&PathBuf::from("/home/u/Id_Rsa")));
        assert!(is_secret_path(&PathBuf::from("/home/u/.SSH/known_hosts")));
        assert!(is_secret_path(&PathBuf::from("/p/Server.PEM")));
    }

    #[test]
    fn flags_expanded_denylist_entries() {
        for p in [
            "/home/u/.git-credentials",
            "/home/u/.kube/config",
            "/home/u/.pgpass",
            "/home/u/.dockercfg",
            "/home/u/.docker/config.json",
            "/home/u/.terraformrc",
            "/home/u/.pypirc",
            "/srv/www/.htpasswd",
            "/home/u/.netrc",
            "/home/u/.config/gh/hosts.yml",
        ] {
            assert!(is_secret_path(&PathBuf::from(p)), "{p} must be flagged");
        }
    }

    #[test]
    fn public_certs_are_not_flagged() {
        assert!(!is_secret_path(&PathBuf::from("/p/server.crt")));
        assert!(!is_secret_path(&PathBuf::from("/p/ca.cer")));
    }

    #[test]
    fn glob_patterns_are_not_screened_but_literal_secret_paths_still_are() {
        let cwd = PathBuf::from("/proj");
        // Globs used as find/ls args touch no literal file → allowed.
        for cmd in ["find . -name '*.pem'", "ls *.key", "rg -g '*.env' TODO"] {
            assert_eq!(
                find_secret_path_in_command(cmd, &cwd, None).unwrap(),
                None,
                "{cmd} should not be screened"
            );
        }
        // A literal secret path is still refused.
        assert!(find_secret_path_in_command("cat /home/u/.ssh/id_rsa", &cwd, Some(Path::new("/home/u")))
            .unwrap()
            .is_some());
        assert!(find_secret_path_in_command("cat .env", &cwd, None).unwrap().is_some());
    }

    #[test]
    fn resolved_check_follows_symlinks() {
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("id_rsa");
        std::fs::write(&secret, "PRIVATE").unwrap();
        let project = tempfile::tempdir().unwrap();
        let link = project.path().join("notes.txt");
        std::os::unix::fs::symlink(&secret, &link).unwrap();

        assert!(!is_secret_path(&link));
        assert!(is_secret_path_resolved(&link));
        assert!(!is_secret_path_resolved(&project.path().join("plain.txt")));
    }

    #[test]
    fn resolved_check_is_lenient_for_missing_files_under_a_symlinked_dir() {
        let outside = tempfile::tempdir().unwrap();
        let ssh = outside.path().join(".ssh");
        std::fs::create_dir(&ssh).unwrap();
        let project = tempfile::tempdir().unwrap();
        let link = project.path().join("keys");
        std::os::unix::fs::symlink(&ssh, &link).unwrap();

        assert!(is_secret_path_resolved(&link.join("brand_new_file")));
    }
}
