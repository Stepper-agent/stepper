use std::fs;
use std::path::Path;
use stepper_config::{Config, SettingsFile};

fn write_command(dir: &Path, file: &str, content: &str) {
    let commands = dir.join("commands");
    fs::create_dir_all(&commands).unwrap();
    fs::write(commands.join(file), content).unwrap();
}

fn config_with_dirs(project_dir: Option<&Path>, user_dir: Option<&Path>) -> Config {
    Config {
        settings: SettingsFile::default(),
        project_dir: project_dir.map(Path::to_path_buf),
        project_root: project_dir.and_then(|d| d.parent().map(Path::to_path_buf)),
        user_dir: user_dir.map(Path::to_path_buf),
    }
}

#[test]
fn command_descriptions_reads_frontmatter_and_tolerates_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join(".stepper");
    write_command(&project, "foo.md", "---\ndescription: do the foo\n---\nbody");
    // No frontmatter description → empty string, still listed.
    write_command(&project, "bar.md", "just a body, no frontmatter");

    let cfg = config_with_dirs(Some(&project), None);
    let descs = cfg.command_descriptions();
    assert_eq!(descs.iter().find(|(n, _)| n == "foo").unwrap().1, "do the foo");
    assert_eq!(descs.iter().find(|(n, _)| n == "bar").unwrap().1, "");
}

#[test]
fn command_descriptions_project_wins_over_user_on_name_clash() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("proj/.stepper");
    let user = tmp.path().join("user/.stepper");
    write_command(&project, "greet.md", "---\ndescription: project greet\n---\nbody");
    write_command(&user, "greet.md", "---\ndescription: user greet\n---\nbody");
    write_command(&user, "only-user.md", "---\ndescription: user only\n---\nbody");

    let cfg = config_with_dirs(Some(&project), Some(&user));
    let descs = cfg.command_descriptions();
    assert_eq!(descs.iter().find(|(n, _)| n == "greet").unwrap().1, "project greet");
    assert_eq!(descs.iter().find(|(n, _)| n == "only-user").unwrap().1, "user only");
}
