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

#[test]
fn json_flag_emits_valid_json_to_stdout() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("a.bin"), b"hello").expect("write file");

    let output = run(&["--json", dir.path().to_str().expect("utf8 path")]);

    assert!(
        output.status.success(),
        "--json must exit zero, got {:?} with stderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    // stdout parses as JSON with a "root" object — machine-readable end to end.
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout must be valid JSON");
    assert!(
        value.get("root").is_some(),
        "JSON payload has a root: {value}"
    );
    assert!(
        value.get("skipped").is_some(),
        "JSON payload has skipped: {value}"
    );
}

/// Runs the same scan twice — once as JSON, once as the human report — and
/// checks they describe the same bytes. A file sized to an exact power of
/// 1024 makes the human report's rounding deterministic ("1.0M"), and
/// `--apparent` sidesteps filesystem block-size differences that would make
/// `allocated` non-portable across machines.
#[test]
fn json_sizes_match_the_human_reports_numbers_for_the_same_scan() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("big.bin"), vec![b'x'; 1_048_576]).expect("write file");
    let root = dir.path().to_str().expect("utf8 path");

    let json_output = run(&["--json", "--apparent", root]);
    let human_output = run(&["--apparent", root]);

    assert!(json_output.status.success(), "{:?}", json_output.status);
    assert!(human_output.status.success(), "{:?}", human_output.status);

    let value: serde_json::Value =
        serde_json::from_slice(&json_output.stdout).expect("stdout must be valid JSON");
    let children = value["root"]["children"]
        .as_array()
        .expect("children array");
    let big = children
        .iter()
        .find(|c| {
            c["path"]
                .as_str()
                .expect("path is a string")
                .ends_with("big.bin")
        })
        .expect("big.bin entry present in JSON");
    assert_eq!(
        big["apparent"], 1_048_576,
        "JSON must carry the raw byte count, not a formatted string"
    );

    let human = String::from_utf8_lossy(&human_output.stdout);
    let file_line = human
        .lines()
        .find(|l| l.contains("big.bin"))
        .unwrap_or_else(|| panic!("big.bin in human report:\n{human}"));
    assert!(
        file_line.contains("1.0M"),
        "the human report must format the same 1_048_576 bytes the JSON carries raw, got:\n{file_line}"
    );
}

/// A real permission-denied directory, reached through the actual binary
/// rather than a hand-built `ScanTree`, must land in `--json`'s `skipped`
/// list — proving the whole pipeline (walk → aggregate → serialize) carries
/// it through, not just the serializer in isolation.
#[cfg(unix)]
#[test]
fn json_includes_a_skipped_entry_from_a_real_unreadable_directory() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("tempdir");
    let locked = dir.path().join("locked");
    std::fs::create_dir(&locked).expect("mkdir");
    std::fs::write(locked.join("secret.bin"), b"hush").expect("write file");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).expect("chmod");

    // `chmod 000` doesn't stop root (e.g. inside a container); a test that
    // passes because the fixture failed to lock is worse than no test.
    if std::fs::read_dir(&locked).is_ok() {
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).expect("restore");
        eprintln!("skipping: running with privileges that ignore chmod 000");
        return;
    }

    let output = run(&["--json", dir.path().to_str().expect("utf8 path")]);

    // Restore before any assertion can panic, so the tempdir can clean up.
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).expect("restore");

    assert!(
        output.status.success(),
        "a skip must not fail the scan, got {:?} with stderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout must be valid JSON");
    let skipped = value["skipped"].as_array().expect("skipped array");
    assert!(
        skipped.iter().any(|s| s["path"]
            .as_str()
            .expect("path is a string")
            .ends_with("locked")
            && s["reason"] == "PermissionDenied"),
        "the unreadable directory must appear in JSON skipped, got: {skipped:?}"
    );
}
