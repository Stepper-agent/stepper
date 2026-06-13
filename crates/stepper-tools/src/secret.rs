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
