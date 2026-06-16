//! End-to-end check that the macOS Seatbelt wrapper actually confines `bash`
//! writes to the writable root. Runs only on macOS, and skips gracefully when
//! `/usr/bin/sandbox-exec` is unavailable (so a stripped host doesn't fail CI).
//!
//! The "outside" target lives under `CARGO_TARGET_TMPDIR` (the project's target
//! dir), NOT under `/tmp`/`$TMPDIR` — `confine_argv` folds the temp dirs into the
//! writable set, so a sibling there is the reliable not-writable location.
#![cfg(target_os = "macos")]

use std::path::PathBuf;
use std::process::Command;

use stepper_tools::sandbox::confine_argv;

fn spawn(argv: &[String]) -> std::process::ExitStatus {
    let (program, rest) = argv.split_first().expect("argv has a program");
    Command::new(program)
        .args(rest)
        .status()
        .expect("spawn confined command")
}

#[test]
fn seatbelt_confines_bash_writes_to_the_writable_root() {
    let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("sandbox_confinement");
    let inside = base.join("inside");
    std::fs::create_dir_all(&inside).expect("create writable root");
    let inside_file = inside.join("ok.txt");
    let outside_file = base.join("outside.txt"); // sibling of `inside`, not under it
    let _ = std::fs::remove_file(&inside_file);
    let _ = std::fs::remove_file(&outside_file);

    let write_inside = confine_argv(
        vec![
            "bash".into(),
            "-c".into(),
            format!("echo ok > {}", inside_file.display()),
        ],
        std::slice::from_ref(&inside),
    );

    // No `/usr/bin/sandbox-exec` on this host → argv is returned unchanged and the
    // confinement property can't be asserted. Skip rather than fail.
    if write_inside.first().map(String::as_str) != Some("/usr/bin/sandbox-exec") {
        eprintln!("skipping: sandbox-exec unavailable, confinement not asserted");
        return;
    }

    assert!(
        spawn(&write_inside).success(),
        "a write inside the writable root should succeed under the sandbox"
    );
    assert!(
        inside_file.exists(),
        "the file inside the writable root should have been created"
    );

    let write_outside = confine_argv(
        vec![
            "bash".into(),
            "-c".into(),
            format!("echo no > {}", outside_file.display()),
        ],
        std::slice::from_ref(&inside),
    );
    // The shell exits non-zero (EPERM); the security property is the filesystem
    // effect, so assert the file was never created.
    let _ = spawn(&write_outside);
    assert!(
        !outside_file.exists(),
        "a write outside the writable root must be denied by the OS sandbox"
    );
}
