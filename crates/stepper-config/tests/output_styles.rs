use std::fs;
use std::path::Path;
use stepper_config::{Config, SettingsFile};

fn write_style(dir: &Path, file: &str, content: &str) {
    let styles = dir.join("output-styles");
    fs::create_dir_all(&styles).unwrap();
    fs::write(styles.join(file), content).unwrap();
}

fn config_with_dirs(
    settings: SettingsFile,
    project_dir: Option<&Path>,
    user_dir: Option<&Path>,
) -> Config {
    Config {
        settings,
        project_dir: project_dir.map(Path::to_path_buf),
        project_root: project_dir.and_then(|d| d.parent().map(Path::to_path_buf)),
        user_dir: user_dir.map(Path::to_path_buf),
    }
}

#[test]
fn loads_styles_with_frontmatter_and_body_sorted_by_name() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join(".stepper");
    write_style(
        &project,
        "explanatory.md",
        "---\nname: Explanatory\ndescription: teaches while coding\n---\nExplain each change.\n",
    );
    write_style(&project, "terse.md", "Answer in one line.\n");

    let styles = config_with_dirs(SettingsFile::default(), Some(&project), None).output_styles();
    assert_eq!(styles.len(), 2);
    assert_eq!(styles[0].name, "Explanatory");
    assert_eq!(styles[0].description.as_deref(), Some("teaches while coding"));
    assert_eq!(styles[0].body.trim(), "Explain each change.");
    assert_eq!(styles[1].name, "terse");
    assert_eq!(styles[1].description, None);
    assert_eq!(styles[1].body.trim(), "Answer in one line.");
}

#[test]
fn project_style_wins_over_user_style_with_same_name() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("proj/.stepper");
    let user = tmp.path().join("home/.stepper");
    write_style(&project, "verbose.md", "project body\n");
    write_style(&user, "verbose.md", "user body\n");
    write_style(&user, "user-only.md", "only in user\n");

    let styles =
        config_with_dirs(SettingsFile::default(), Some(&project), Some(&user)).output_styles();
    assert_eq!(styles.len(), 2);
    assert_eq!(styles[0].name, "user-only");
    assert_eq!(styles[1].name, "verbose");
    assert_eq!(styles[1].body.trim(), "project body");
}

#[test]
fn missing_output_styles_dir_yields_empty_list() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join(".stepper");
    fs::create_dir_all(&project).unwrap();
    let cfg = config_with_dirs(SettingsFile::default(), Some(&project), None);
    assert!(cfg.output_styles().is_empty());
}

#[test]
fn settings_parse_output_style_field() {
    let settings: SettingsFile =
        serde_json::from_str(r#"{"outputStyle":"Explanatory"}"#).unwrap();
    assert_eq!(settings.output_style.as_deref(), Some("Explanatory"));
    assert_eq!(SettingsFile::default().output_style, None);
}

#[test]
fn validate_values_flags_output_style_naming_no_style() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join(".stepper");
    write_style(&project, "terse.md", "Answer in one line.\n");

    let settings: SettingsFile =
        serde_json::from_str(r#"{"outputStyle":"missing"}"#).unwrap();
    let problems = config_with_dirs(settings, Some(&project), None).validate_values();
    assert_eq!(problems.len(), 1);
    assert!(problems[0].contains("outputStyle"), "got: {}", problems[0]);
    assert!(problems[0].contains("'missing'"), "got: {}", problems[0]);
    assert!(problems[0].contains("terse"), "lists available styles: {}", problems[0]);
}

#[test]
fn validate_values_accepts_existing_output_style() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join(".stepper");
    write_style(&project, "terse.md", "Answer in one line.\n");

    let settings: SettingsFile = serde_json::from_str(r#"{"outputStyle":"terse"}"#).unwrap();
    let problems = config_with_dirs(settings, Some(&project), None).validate_values();
    assert_eq!(problems, Vec::<String>::new());
}

#[test]
fn validate_values_reports_bodiless_style_file() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join(".stepper");
    write_style(&project, "empty.md", "---\nname: empty\n---\n  \n");

    let problems =
        config_with_dirs(SettingsFile::default(), Some(&project), None).validate_values();
    assert_eq!(problems.len(), 1);
    assert!(problems[0].contains("output-style/empty"), "got: {}", problems[0]);
}

#[test]
fn validate_values_flags_bad_layer_on_failure_frontmatter() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join(".stepper");
    let layer = project.join("layer/impl");
    fs::create_dir_all(&layer).unwrap();
    fs::write(
        layer.join("index.md"),
        "---\ndescription: implement\non-failure: skipp\n---\nbody\n",
    )
    .unwrap();

    let problems =
        config_with_dirs(SettingsFile::default(), Some(&project), None).validate_values();
    assert_eq!(problems.len(), 1);
    assert!(problems[0].contains("layer/impl"), "got: {}", problems[0]);
    assert!(problems[0].contains("skipp"), "got: {}", problems[0]);
}

#[test]
fn validate_values_accepts_known_layer_on_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join(".stepper");
    for (name, policy) in [("a", "stop"), ("b", "skip")] {
        let layer = project.join("layer").join(name);
        fs::create_dir_all(&layer).unwrap();
        fs::write(
            layer.join("index.md"),
            format!("---\ndescription: d\non-failure: {policy}\n---\nbody\n"),
        )
        .unwrap();
    }

    let problems =
        config_with_dirs(SettingsFile::default(), Some(&project), None).validate_values();
    assert_eq!(problems, Vec::<String>::new());
}

#[test]
fn validate_values_reports_unparseable_layer_index() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join(".stepper");
    let layer = project.join("layer/broken");
    fs::create_dir_all(&layer).unwrap();
    fs::write(layer.join("index.md"), "---\nmodel: x/y\n---\nbody\n").unwrap();

    let problems =
        config_with_dirs(SettingsFile::default(), Some(&project), None).validate_values();
    assert_eq!(problems.len(), 1);
    assert!(problems[0].contains("layer/broken"), "got: {}", problems[0]);
}

#[test]
fn validate_values_combines_settings_and_file_problems() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join(".stepper");
    write_style(&project, "terse.md", "Answer in one line.\n");

    let settings: SettingsFile = serde_json::from_str(
        r#"{"providers":{"typo":{"kind":"openai-compt"}},"outputStyle":"missing"}"#,
    )
    .unwrap();
    let problems = config_with_dirs(settings, Some(&project), None).validate_values();
    assert_eq!(problems.len(), 2);
    assert!(problems[0].contains("providers.typo.kind"), "got: {}", problems[0]);
    assert!(problems[1].contains("outputStyle"), "got: {}", problems[1]);
}
