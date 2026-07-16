//! End-to-end checks of the built binary's argument handling and exit codes.
//!
//! Spawns the real executable (`CARGO_BIN_EXE_disk-tools`, provided by Cargo to
//! integration tests) rather than calling into the crate, so exit codes and
//! stderr are exercised exactly as a user would see them.

use std::path::Path;
use std::process::Command;

fn run(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_disk-tools"))
        .args(args)
        .output()
        .expect("spawn disk-tools")
}

#[test]
fn missing_path_errors_and_does_not_scan_cwd() {
    let output = run(&[]);

    assert!(
        !output.status.success(),
        "no path must exit non-zero, got {:?}",
        output.status
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.to_lowercase().contains("usage"),
        "a missing path should print usage, got:\n{stderr}"
    );
}

#[test]
fn nonexistent_path_errors_early() {
    // A path that cannot exist under a temp dir, so nothing is scanned.
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("does-not-exist");

    let output = run(&[missing.to_str().expect("utf8 path")]);

    assert!(
        !output.status.success(),
        "a nonexistent path must exit non-zero, got {:?}",
        output.status
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(&missing.display().to_string()),
        "the error should name the missing path, got:\n{stderr}"
    );
}

#[test]
fn valid_path_exits_zero() {
    let dir = tempfile::tempdir().expect("tempdir");

    let output = run(&[dir.path().to_str().expect("utf8 path")]);

    assert!(
        output.status.success(),
        "a valid path must exit zero, got {:?} with stderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn valid_path_produces_empty_stderr() {
    let dir = tempfile::tempdir().expect("tempdir");

    let output = run(&[dir.path().to_str().expect("utf8 path")]);

    assert!(
        output.stderr.is_empty(),
        "success must not print to stderr, got:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// clap's own usage errors (e.g. a missing required positional) exit with
/// code 2, matching the code `validate_root`'s failure path returns by hand
/// in `main.rs` — the two error sources agree, not just "non-zero".
#[test]
fn missing_path_exits_with_code_2() {
    let output = run(&[]);

    assert_eq!(
        output.status.code(),
        Some(2),
        "usage errors must exit with code 2, got {:?}",
        output.status
    );
}

#[test]
fn nonexistent_path_exits_with_code_2() {
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("does-not-exist");

    let output = run(&[missing.to_str().expect("utf8 path")]);

    assert_eq!(
        output.status.code(),
        Some(2),
        "a nonexistent path must exit with code 2, got {:?}",
        output.status
    );
}

/// The binary path Cargo hands us must actually point at an executable — guards
/// against the env var silently going missing.
#[test]
fn binary_under_test_exists() {
    assert!(Path::new(env!("CARGO_BIN_EXE_disk-tools")).exists());
}
