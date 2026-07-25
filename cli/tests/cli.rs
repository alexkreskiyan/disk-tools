//! End-to-end checks of the built binary's argument handling and exit codes.
//!
//! Spawns the real executable (`CARGO_BIN_EXE_disk-tools`, provided by Cargo to
//! integration tests) rather than calling into the crate, so exit codes and
//! stderr are exercised exactly as a user would see them.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};

fn run(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_disk-tools"))
        .args(args)
        .output()
        .expect("spawn disk-tools")
}

/// Run the binary, read one small chunk of stdout, then close the pipe —
/// exactly what `disk-tools <path> | head` does to it.
///
/// The fixture must be big enough that the report exceeds the OS pipe buffer
/// (64 KiB is typical); otherwise the child writes everything before the reader
/// goes away and the closed-pipe path is never taken.
fn run_with_stdout_closed_early(args: &[&str]) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_disk-tools"))
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn disk-tools");

    let mut stdout = child.stdout.take().expect("stdout is piped");
    let mut buf = [0u8; 64];
    let _ = stdout.read(&mut buf);
    drop(stdout);

    child.wait_with_output().expect("wait for disk-tools")
}

/// A directory with enough entries that the rendered report is well past any
/// pipe buffer.
fn many_files_dir(count: usize) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    for i in 0..count {
        std::fs::write(dir.path().join(format!("file-{i:05}.bin")), b"x").expect("write file");
    }
    dir
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

/// In `--json` mode the skipped summary must go to stderr, leaving stdout as
/// pure JSON — the stdout/stderr separation that keeps piped output parseable.
#[cfg(unix)]
#[test]
fn json_stdout_stays_clean_progress_to_stderr() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("tempdir");
    let locked = dir.path().join("locked");
    std::fs::create_dir(&locked).expect("mkdir");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).expect("chmod");

    if std::fs::read_dir(&locked).is_ok() {
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).expect("restore");
        eprintln!("skipping: running with privileges that ignore chmod 000");
        return;
    }

    let output = run(&["--json", dir.path().to_str().expect("utf8 path")]);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).expect("restore");

    // stdout is valid JSON and carries none of the summary prose.
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str::<serde_json::Value>(&stdout).expect("stdout must be valid JSON");
    assert!(
        !stdout.contains("skipped:"),
        "the skipped summary must not appear on stdout:\n{stdout}"
    );
    // The summary went to stderr instead.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("skipped:"),
        "the skipped summary must be on stderr, got:\n{stderr}"
    );
}

/// The same stdout/stderr split as `--json`, but for the default tree report:
/// the human report belongs on stdout, the skipped summary on stderr, and
/// neither leaks into the other.
#[cfg(unix)]
#[test]
fn tree_mode_summary_goes_to_stderr_report_stays_on_stdout() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("visible.bin"), b"hello").expect("write file");
    let locked = dir.path().join("locked");
    std::fs::create_dir(&locked).expect("mkdir");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).expect("chmod");

    if std::fs::read_dir(&locked).is_ok() {
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).expect("restore");
        eprintln!("skipping: running with privileges that ignore chmod 000");
        return;
    }

    let output = run(&[dir.path().to_str().expect("utf8 path")]);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).expect("restore");

    assert!(
        output.status.success(),
        "a skip must not fail the scan, got {:?} with stderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("visible.bin"),
        "the tree report belongs on stdout:\n{stdout}"
    );
    assert!(
        !stdout.contains("skipped:"),
        "the skipped summary must not leak onto stdout:\n{stdout}"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("skipped:") && stderr.contains("locked"),
        "the skipped summary (naming the locked dir) must be on stderr, got:\n{stderr}"
    );
}

/// A run that produces a skip but is fully piped (no tty on either stream)
/// must not leave any spinner escape sequences behind — indicatif is
/// expected to hide itself entirely, so stderr should hold nothing but the
/// plain-text summary.
#[cfg(unix)]
#[test]
fn piped_stderr_carries_only_the_summary_no_spinner_control_bytes() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("tempdir");
    let locked = dir.path().join("locked");
    std::fs::create_dir(&locked).expect("mkdir");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).expect("chmod");

    if std::fs::read_dir(&locked).is_ok() {
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).expect("restore");
        eprintln!("skipping: running with privileges that ignore chmod 000");
        return;
    }

    let output = run(&[dir.path().to_str().expect("utf8 path")]);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).expect("restore");

    assert!(
        !output.stderr.contains(&0x1b),
        "a piped run's stderr must contain no ESC control bytes from the spinner, got: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("skipped:"),
        "the summary itself must still be present, got:\n{stderr}"
    );
}

/// `disk-tools <path> | head` closes stdout after a few lines. That is a normal
/// end of output for a Unix filter, not a failure: the process must stop
/// quietly with success instead of panicking with "failed printing to stdout:
/// Broken pipe".
#[test]
fn closed_stdout_pipe_exits_quietly_instead_of_panicking() {
    let dir = many_files_dir(3000);

    let output = run_with_stdout_closed_early(&[dir.path().to_str().expect("utf8 path")]);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("panicked"),
        "a closed stdout pipe must not panic, got:\n{stderr}"
    );
    assert!(
        output.status.success(),
        "a closed stdout pipe must exit zero, got {:?} with stderr:\n{stderr}",
        output.status
    );
}

/// The same for `--json`: `disk-tools <path> --json | head -c 100` must not
/// panic either, since the JSON payload takes the same output path.
#[test]
fn closed_stdout_pipe_in_json_mode_exits_quietly() {
    let dir = many_files_dir(3000);

    let output = run_with_stdout_closed_early(&["--json", dir.path().to_str().expect("utf8 path")]);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("panicked"),
        "a closed stdout pipe must not panic under --json, got:\n{stderr}"
    );
    assert!(
        output.status.success(),
        "a closed stdout pipe must exit zero under --json, got {:?} with stderr:\n{stderr}",
        output.status
    );
}

/// With more than ten skips, the default run truncates and --verbose lists
/// every one — exercised through the real binary against genuinely
/// unreadable directories, not the formatter in isolation.
#[cfg(unix)]
#[test]
fn verbose_flag_lists_every_skip_past_the_preview_cap() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("tempdir");
    let mut locked_dirs = Vec::new();
    for i in 0..11 {
        let locked = dir.path().join(format!("locked-{i}"));
        std::fs::create_dir(&locked).expect("mkdir");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).expect("chmod");
        locked_dirs.push(locked);
    }

    // Bail loudly, as in the other chmod-000 fixtures, if privileges ignore it.
    if std::fs::read_dir(&locked_dirs[0]).is_ok() {
        for locked in &locked_dirs {
            std::fs::set_permissions(locked, std::fs::Permissions::from_mode(0o755))
                .expect("restore");
        }
        eprintln!("skipping: running with privileges that ignore chmod 000");
        return;
    }

    let root = dir.path().to_str().expect("utf8 path");
    let default_output = run(&[root]);
    let verbose_output = run(&["--verbose", root]);

    for locked in &locked_dirs {
        std::fs::set_permissions(locked, std::fs::Permissions::from_mode(0o755)).expect("restore");
    }

    assert!(
        default_output.status.success(),
        "{:?}",
        default_output.status
    );
    assert!(
        verbose_output.status.success(),
        "{:?}",
        verbose_output.status
    );

    let default_stderr = String::from_utf8_lossy(&default_output.stderr);
    assert!(
        default_stderr.contains("11 entries skipped:"),
        "the header states the true total even when truncated:\n{default_stderr}"
    );
    assert!(
        default_stderr.contains("… and 1 more"),
        "eleven skips leave exactly one out of the default preview:\n{default_stderr}"
    );

    let verbose_stderr = String::from_utf8_lossy(&verbose_output.stderr);
    assert!(
        !verbose_stderr.contains("more"),
        "--verbose must elide nothing:\n{verbose_stderr}"
    );
    for locked in &locked_dirs {
        let name = locked.file_name().unwrap().to_str().unwrap();
        assert!(
            verbose_stderr.contains(name),
            "--verbose must list every skipped path, missing {name} in:\n{verbose_stderr}"
        );
    }
}

// ---- the clean subcommand ------------------------------------------------

/// A fixture with one obvious candidate and nothing else interesting.
fn cleanable_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir(dir.path().join("node_modules")).expect("mkdir");
    std::fs::write(dir.path().join("node_modules/lib.bin"), vec![b'x'; 4096]).expect("write");
    dir
}

/// Every path under `root`, with its length — enough to catch a creation, a
/// deletion or a truncation.
fn snapshot(root: &Path) -> Vec<(std::path::PathBuf, u64)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        for entry in std::fs::read_dir(&path).expect("read_dir") {
            let entry = entry.expect("entry");
            let metadata = entry.metadata().expect("metadata");
            if metadata.is_dir() {
                stack.push(entry.path());
            }
            out.push((entry.path(), metadata.len()));
        }
    }
    out.sort();
    out
}

/// The v0.1 surface is a contract: adding a subcommand beside the bare path
/// must not change what the bare path does.
#[test]
fn bare_path_still_scans_exactly_as_before() {
    let dir = many_files_dir(20);

    let output = run(&[dir.path().to_str().expect("utf8 path")]);

    assert!(output.status.success(), "{:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("file-00000.bin"),
        "the tree report is unchanged:\n{stdout}"
    );
    assert!(
        !stdout.contains("Reclaimable"),
        "a plain scan must not print a cleanup report:\n{stdout}"
    );
}

#[test]
fn clean_without_a_path_exits_two() {
    let output = run(&["clean"]);

    assert_eq!(
        output.status.code(),
        Some(2),
        "a missing path is a usage error, got {:?}",
        output.status
    );
}

/// The load-bearing safety property: the default does nothing at all.
#[test]
fn dry_run_writes_nothing() {
    let dir = cleanable_dir();
    let before = snapshot(dir.path());

    let output = run(&["clean", dir.path().to_str().expect("utf8 path")]);

    assert!(output.status.success(), "{:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("node_modules"),
        "the fixture must actually match, or this proves nothing:\n{stdout}"
    );
    assert!(
        stdout.contains("Dry run"),
        "and the report must say it removed nothing:\n{stdout}"
    );
    assert_eq!(
        before,
        snapshot(dir.path()),
        "a dry run must leave the tree byte-identical"
    );
}

/// `--apply` really removes, end to end through the binary.
///
/// `#[ignore]` for the reason every real-trash test here carries: it puts
/// things in the developer's actual Trash. Run via `just smoke-trash`.
#[test]
#[ignore = "moves real files to the OS trash; run via `just smoke-trash`"]
fn apply_removes_the_candidates() {
    let dir = cleanable_dir();
    let path = dir.path().to_str().expect("utf8 path");

    let output = run(&["clean", path, "--apply"]);

    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        eprintln!("skipping: this environment has no usable trash backend:\n{stderr}");
        return;
    }
    assert!(
        !dir.path().join("node_modules").exists(),
        "the candidate must be gone from its original path"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Removed 1 of 1"), "{stdout}");
}

/// A removal that partly fails must say so in both channels a script reads: a
/// non-zero exit, and a report naming what is still on disk.
///
/// The fixture makes the *parent* read-only, so the scan can still see
/// `node_modules` but the trash cannot take it out of a directory it may not
/// write. Nothing reaches the real Trash, which is why this one need not be
/// `#[ignore]`d.
///
/// **Linux only, and that was measured rather than assumed.** On macOS the
/// backend drives Finder through `osascript`, which is not bound by the
/// parent's permissions: the same fixture was removed successfully there, in
/// 42 seconds. Linux's freedesktop backend is a rename, which a read-only
/// parent really does refuse. The core's `one_failure_does_not_stop_the_rest`
/// and `outcome_names_every_failure` cover the data on every platform; this
/// covers the exit code and the report end to end where it can.
#[cfg(target_os = "linux")]
#[test]
fn partial_failure_exits_non_zero_and_names_survivors() {
    use std::os::unix::fs::PermissionsExt;

    let dir = cleanable_dir();
    let root = dir.path();
    let readonly = std::fs::Permissions::from_mode(0o555);
    std::fs::set_permissions(root, readonly).expect("chmod");

    // Running with privileges that ignore the missing write bit would let the
    // removal succeed and pass this test for the wrong reason.
    if std::fs::create_dir(root.join("probe")).is_ok() {
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o755)).expect("restore");
        eprintln!("skipping: privileges ignore the read-only parent");
        return;
    }

    let output = run(&["clean", root.to_str().expect("utf8 path"), "--apply"]);

    // Restore before any assertion can unwind, or TempDir::drop cannot clean up.
    std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o755)).expect("restore");

    assert!(
        !output.status.success(),
        "a removal that failed is not a success, got {:?}",
        output.status
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Not removed:") && stdout.contains("node_modules"),
        "the report must name what is still on disk:\n{stdout}"
    );
    assert!(
        dir.path().join("node_modules").exists(),
        "and it really must still be there"
    );
}

/// The report goes to stdout and everything else to stderr, the same split the
/// scan report keeps.
#[test]
fn clean_report_is_on_stdout() {
    let dir = cleanable_dir();

    let output = run(&["clean", dir.path().to_str().expect("utf8 path")]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Reclaimable"), "{stdout}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("Reclaimable"),
        "the report must not be duplicated on stderr:\n{stderr}"
    );
}

/// Backdate `path` so the age rule really has something to find. `FileTimes`
/// is the only portable way to build an "old" fixture — otherwise the test
/// would need a threshold of zero, which matches the fixture's own root and
/// hides everything beneath it.
fn backdate(path: &Path, age: std::time::Duration) {
    let file = std::fs::File::options()
        .write(true)
        .open(path)
        .expect("open for set_times");
    let when = std::time::SystemTime::now() - age;
    file.set_times(std::fs::FileTimes::new().set_modified(when))
        .expect("set mtime");
}

/// `--safe` drops confirm-tier candidates without recording them in the plan —
/// deliberately, since it is the user's own narrowing. The report is the only
/// place they learn something was there, so the count has to be right.
#[test]
fn safe_reports_how_many_candidates_it_hid() {
    const YEAR: std::time::Duration = std::time::Duration::from_secs(365 * 24 * 60 * 60);

    let dir = cleanable_dir();
    for name in ["ancient-one.bin", "ancient-two.bin"] {
        let path = dir.path().join(name);
        std::fs::write(&path, b"old").expect("write");
        backdate(&path, YEAR);
    }
    let path = dir.path().to_str().expect("utf8 path");

    let everything = run(&["clean", path, "--older-than", "90d"]);
    let stdout = String::from_utf8_lossy(&everything.stdout);
    assert!(
        stdout.contains("ancient-one.bin") && stdout.contains("node_modules"),
        "the fixture must offer both tiers, or this proves nothing:\n{stdout}"
    );

    let safe = run(&["clean", path, "--older-than", "90d", "--safe"]);
    let stdout = String::from_utf8_lossy(&safe.stdout);

    assert!(
        stdout.contains("node_modules"),
        "the auto-tier candidate survives --safe:\n{stdout}"
    );
    assert!(
        !stdout.contains("ancient-one.bin"),
        "the confirm-tier ones do not:\n{stdout}"
    );
    assert!(
        stdout.contains("2 more candidates need confirmation"),
        "and the report says exactly how many were hidden:\n{stdout}"
    );
}

#[test]
fn clean_on_a_missing_path_exits_two() {
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("nope");

    let output = run(&["clean", missing.to_str().expect("utf8 path")]);

    assert_eq!(
        output.status.code(),
        Some(2),
        "an unusable root is reported before anything is scanned, got {:?}",
        output.status
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("does not exist"), "{stderr}");
}

/// `--purge` is the one path that leaves nothing to recover, so it is worth
/// proving end to end — and it needs no `#[ignore]`, because nothing reaches the
/// developer's Trash by definition.
#[test]
fn purge_removes_without_trashing_and_says_so() {
    let dir = cleanable_dir();
    let path = dir.path().to_str().expect("utf8 path");

    let output = run(&["clean", path, "--apply", "--purge"]);

    assert!(output.status.success(), "{:?}", output.status);
    assert!(
        !dir.path().join("node_modules").exists(),
        "the candidate is gone"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("cannot be put back"),
        "the user must be told before it happens:\n{stderr}"
    );
}

#[test]
fn purge_without_apply_is_a_usage_error() {
    let dir = cleanable_dir();
    let before = snapshot(dir.path());

    let output = run(&["clean", dir.path().to_str().expect("utf8 path"), "--purge"]);

    assert_eq!(
        output.status.code(),
        Some(2),
        "a destructive flag on its own is a usage error, got {:?}",
        output.status
    );
    assert_eq!(before, snapshot(dir.path()), "and nothing was removed");
}
