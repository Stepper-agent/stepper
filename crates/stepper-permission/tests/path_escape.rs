//! Path classification: a symlink that escapes the project and a `..` traversal
//! both resolve to their real location and are judged outside the project.

use std::fs;
use std::path::Path;
use stepper_permission::path;
use stepper_permission::{Decision, PermissionMode, PermissionRequest, RuleSet};

#[test]
fn in_project_file_is_inside() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    fs::create_dir_all(project.join("src")).unwrap();
    let file = project.join("src").join("main.rs");
    fs::write(&file, "fn main() {}").unwrap();
    assert!(path::is_in_project(&file, &project));
}

#[test]
fn symlink_escaping_project_is_outside() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    let outside = dir.path().join("outside");
    fs::create_dir_all(&project).unwrap();
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("secret.txt"), "top secret").unwrap();

    let link = project.join("link");
    symlink_dir(&outside, &link);

    let escaped = link.join("secret.txt");
    assert!(!path::is_in_project(&escaped, &project));
}

#[test]
fn parent_dir_traversal_escaping_project_is_outside() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    let outside = dir.path().join("outside");
    fs::create_dir_all(project.join("src")).unwrap();
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("secret.txt"), "top secret").unwrap();

    let traversed = project
        .join("src")
        .join("..")
        .join("..")
        .join("outside")
        .join("secret.txt");
    assert!(!path::is_in_project(&traversed, &project));
}

#[test]
fn parent_dir_traversal_back_into_project_is_inside() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    fs::create_dir_all(project.join("src")).unwrap();
    fs::write(project.join("Cargo.toml"), "[package]").unwrap();

    let back = project.join("src").join("..").join("Cargo.toml");
    assert!(path::is_in_project(&back, &project));
}

#[test]
fn auto_mode_treats_symlink_escape_as_outside_and_asks() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    let outside = dir.path().join("outside");
    fs::create_dir_all(&project).unwrap();
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("secret.txt"), "top secret").unwrap();

    let link = project.join("link");
    symlink_dir(&outside, &link);

    let rules = RuleSet::default();
    let request = PermissionRequest::Read(link.join("secret.txt"));
    assert_eq!(
        stepper_permission::evaluate(&request, &rules, &project, None, PermissionMode::Auto),
        Decision::Ask
    );
}

#[test]
fn relative_in_project_path_is_inside() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    fs::create_dir_all(project.join("src")).unwrap();
    fs::write(project.join("src").join("lib.rs"), "").unwrap();
    assert!(path::is_in_project(Path::new("src/lib.rs"), &project));
}

#[test]
fn home_anchored_rule_matches_path_under_home_via_evaluate() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    let home = dir.path().join("home");
    fs::create_dir_all(&project).unwrap();
    fs::create_dir_all(home.join(".config")).unwrap();
    let target = home.join(".config").join("settings.toml");
    fs::write(&target, "k = 1").unwrap();

    let rules = RuleSet::from_lists(&["Read(~/.config/**)".into()], &[], &[]);

    assert_eq!(
        stepper_permission::evaluate(
            &PermissionRequest::Read(target.clone()),
            &rules,
            &project,
            Some(home.as_path()),
            PermissionMode::Auto,
        ),
        Decision::Allow
    );
}

#[test]
fn home_anchored_rule_does_not_match_path_outside_home_via_evaluate() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    let home = dir.path().join("home");
    let elsewhere = dir.path().join("elsewhere");
    fs::create_dir_all(&project).unwrap();
    fs::create_dir_all(home.join(".config")).unwrap();
    fs::create_dir_all(&elsewhere).unwrap();
    let outside = elsewhere.join("settings.toml");
    fs::write(&outside, "k = 1").unwrap();

    let rules = RuleSet::from_lists(&["Read(~/.config/**)".into()], &[], &[]);

    assert_eq!(
        stepper_permission::evaluate(
            &PermissionRequest::Read(outside),
            &rules,
            &project,
            Some(home.as_path()),
            PermissionMode::Auto,
        ),
        Decision::Ask
    );
}

#[test]
fn home_anchored_rule_without_home_does_not_match() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    let home = dir.path().join("home");
    fs::create_dir_all(&project).unwrap();
    fs::create_dir_all(home.join(".config")).unwrap();
    let target = home.join(".config").join("settings.toml");
    fs::write(&target, "k = 1").unwrap();

    let rules = RuleSet::from_lists(&["Read(~/.config/**)".into()], &[], &[]);

    assert_eq!(
        stepper_permission::evaluate(
            &PermissionRequest::Read(target),
            &rules,
            &project,
            None,
            PermissionMode::Auto,
        ),
        Decision::Ask
    );
}

#[cfg(unix)]
fn symlink_dir(target: &Path, link: &Path) {
    std::os::unix::fs::symlink(target, link).unwrap();
}

#[cfg(windows)]
fn symlink_dir(target: &Path, link: &Path) {
    std::os::windows::fs::symlink_dir(target, link).unwrap();
}
