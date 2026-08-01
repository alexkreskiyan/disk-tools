//! End-to-end checks of the built binary's argument handling and exit codes.
//!
//! Spawns the real executable (`CARGO_BIN_EXE_disk-tools`, provided by Cargo to
//! integration tests) rather than calling into the crate, so exit codes and
//! stderr are exercised exactly as a user would see them.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};

/// An empty directory to stand in for the runner's own home and config.
///
/// Both matter, and both were found the hard way.
///
/// `XDG_CONFIG_HOME`: without it every test reads the config of whoever runs
/// them, and a `depth` or `n` in that file silently changes what the binary
/// prints. Three tests failed that way the moment the config reached behaviour.
///
/// `HOME`: `clean` with no path walks the roots of the rules, and the built-in
/// `user-caches` is rooted at the home directory. Without this the suite scanned
/// the developer's entire home — 93 seconds, and every unreadable directory in
/// it reported as a skip.
fn isolated() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

fn spawn(args: &[&str], home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_disk-tools"));
    command
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env_remove("LOCALAPPDATA")
        .env_remove("APPDATA")
        .args(args);
    command
}

fn run(args: &[&str]) -> std::process::Output {
    let home = isolated();
    spawn(args, home.path()).output().expect("spawn disk-tools")
}

/// Run the binary, read one small chunk of stdout, then close the pipe —
/// exactly what `disk-tools <path> | head` does to it.
///
/// The fixture must be big enough that the report exceeds the OS pipe buffer
/// (64 KiB is typical); otherwise the child writes everything before the reader
/// goes away and the closed-pipe path is never taken.
fn run_with_stdout_closed_early(args: &[&str]) -> std::process::Output {
    let home = isolated();
    let mut child = spawn(args, home.path())
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
    assert_eq!(output.status.code(), Some(2), "a usage error, not a crash");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.to_lowercase().contains("usage"),
        "a bare invocation should print usage, got:\n{stderr}"
    );
    // The point of printing help rather than an error: it says what to type.
    for verb in ["scan", "preview", "clean"] {
        assert!(
            stderr.contains(verb),
            "and it should list `{verb}`, got:\n{stderr}"
        );
    }
    assert!(
        String::from_utf8_lossy(&output.stdout).is_empty(),
        "nothing was scanned, so stdout stays empty"
    );
}

#[test]
fn nonexistent_path_errors_early() {
    // A path that cannot exist under a temp dir, so nothing is scanned.
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("does-not-exist");

    let output = run(&["scan", missing.to_str().expect("utf8 path")]);

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

    let output = run(&["scan", dir.path().to_str().expect("utf8 path")]);

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

    let output = run(&["scan", dir.path().to_str().expect("utf8 path")]);

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

    let output = run(&["scan", missing.to_str().expect("utf8 path")]);

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

    let output = run(&["scan", "--json", dir.path().to_str().expect("utf8 path")]);

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

    let json_output = run(&["scan", "--json", "--apparent", root]);
    let human_output = run(&["scan", "--apparent", root]);

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

    let output = run(&["scan", "--json", dir.path().to_str().expect("utf8 path")]);

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

    let output = run(&["scan", "--json", dir.path().to_str().expect("utf8 path")]);
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

    let output = run(&["scan", dir.path().to_str().expect("utf8 path")]);
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

    let output = run(&["scan", dir.path().to_str().expect("utf8 path")]);
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

    let output = run_with_stdout_closed_early(&["scan", dir.path().to_str().expect("utf8 path")]);

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

    let output =
        run_with_stdout_closed_early(&["scan", "--json", dir.path().to_str().expect("utf8 path")]);

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
    let default_output = run(&["scan", root]);
    let verbose_output = run(&["scan", "--verbose", root]);

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

/// A fixture path in the form the spawned process's own working directory will
/// have, so that a `~`-rooted rule and a relative path can meet.
///
/// Both halves are the platform's doing and neither is the tool's:
///
/// - **macOS** hands `tempdir` a `/var/...` path while `current_dir()` reports
///   the resolved `/private/var/...`, so without canonicalising, the rule's root
///   and the candidate disagree.
/// - **Windows** canonicalises to a *verbatim* path (`\\?\C:\...`), which a
///   working directory never is — so canonicalising alone makes them disagree in
///   the other direction. The prefix is stripped.
///
/// Only a test needs this. The binary must not canonicalise: the path it shows
/// has to be the path it removes, and resolving links would report somewhere the
/// user never named.
fn as_the_child_sees_it(path: &Path) -> std::path::PathBuf {
    let canonical = std::fs::canonicalize(path).expect("canonicalize the fixture");
    let text = canonical.to_string_lossy().into_owned();
    std::path::PathBuf::from(text.strip_prefix(r"\\?\").unwrap_or(&text))
}

/// The bug a preview cannot survive: `preview .` inside a project reported
/// "Nothing to clean" while `preview /full/path` to the same directory found
/// gigabytes.
///
/// A rooted rule is compiled against an absolute root, so a relative path
/// produces nodes like `./node_modules` that no such glob can match — and the
/// run claims nothing, silently and in the safe direction. Only a **rooted**
/// rule shows it: the built-in `node-modules` is unrooted, matches by name
/// wherever it is, and works relative or not.
#[test]
fn a_relative_path_finds_what_the_absolute_one_does() {
    let home = isolated();
    let home_dir = as_the_child_sees_it(home.path());
    let home_dir = home_dir.as_path();
    let project = home_dir.join("project");
    std::fs::create_dir_all(project.join("node_modules")).expect("mkdir");
    std::fs::write(project.join("node_modules/lib.bin"), vec![b'x'; 4096]).expect("write");

    let config = home_dir.join("rules.toml");
    std::fs::write(
        &config,
        "clean-rules:\n  - name: js\n    tier: trash\n    parts:\n      - root: \"~\"\n        includes: [\"**/node_modules/\"]\n",
    )
    .expect("write config");
    let config = config.to_str().expect("utf8");

    let from = |dir: &Path, path: &str| {
        let output = spawn(&["--config", config, "preview", path, "-d", "1"], home_dir)
            .current_dir(dir)
            .output()
            .expect("spawn disk-tools");
        assert!(output.status.success(), "{:?}", output.status);
        String::from_utf8_lossy(&output.stdout).into_owned()
    };

    let relative = from(&project, ".");
    let absolute = from(home_dir, project.to_str().expect("utf8"));

    assert!(
        relative.contains("node_modules"),
        "`preview .` must find what the absolute path finds:\n{relative}"
    );
    assert_eq!(
        relative, absolute,
        "and report it identically, since it is the same directory"
    );
}

/// The one property a machine-readable output has to keep: a display flag
/// cannot change a byte of it. Anything else and a consumer is reading a
/// document that was quietly shortened, with nothing in it saying so.
#[test]
fn display_flags_cannot_change_the_json() {
    let dir = cleanable_dir();
    let path = dir.path().to_str().expect("utf8");
    let plain = run(&["preview", path, "--json"]);
    assert!(plain.status.success(), "{:?}", plain.status);

    for extra in [
        vec!["-d", "1"],
        vec!["-d", "9"],
        vec!["--sort", "size"],
        vec!["-d", "1", "--sort", "size"],
    ] {
        let mut args = vec!["preview", path, "--json"];
        args.extend_from_slice(&extra);
        let output = run(&args);

        assert_eq!(
            output.stdout, plain.stdout,
            "{extra:?} changed the document"
        );
    }
}

/// And a flag that narrows the *plan* is of course reflected: it changes what
/// the answer is, not how it is shown.
#[test]
fn a_narrowing_flag_does_change_the_json() {
    let dir = cleanable_dir();
    let path = dir.path().to_str().expect("utf8");

    let whole = run(&["preview", path, "--json"]);
    let narrowed = run(&["preview", path, "--json", "--min-size", "1G"]);

    let of = |out: &std::process::Output| -> serde_json::Value {
        serde_json::from_slice(&out.stdout).expect("valid JSON")
    };
    assert!(
        !of(&whole)["candidates"]
            .as_array()
            .expect("array")
            .is_empty()
    );
    assert!(
        of(&narrowed)["candidates"]
            .as_array()
            .expect("array")
            .is_empty(),
        "nothing here is a gigabyte"
    );
}

/// stdout is the document and stderr is everything else — which is what makes
/// the output pipeable at all.
#[test]
fn preview_json_is_one_document_on_stdout() {
    let dir = cleanable_dir();

    let output = run(&[
        "preview",
        dir.path().to_str().expect("utf8"),
        "--json",
        "-v",
    ]);

    assert!(output.status.success(), "{:?}", output.status);
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout must be valid JSON");
    let first = &value["candidates"][0];
    assert_eq!(first["rule"], "node-modules");
    assert_eq!(first["tier"], "trash");
    assert_eq!(first["purge"], false);
    assert!(
        first["allocated"]
            .as_u64()
            .is_some_and(|bytes| bytes >= 4096),
        "a raw byte count: {first}"
    );
}

/// `clean --json` answers a different question — what was *done* — so it is a
/// different document, and the two halves are both in it.
#[test]
fn clean_json_reports_what_it_did() {
    let dir = cleanable_dir();

    let output = run(&["clean", dir.path().to_str().expect("utf8"), "--json"]);

    assert!(output.status.success(), "{:?}", output.status);
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout must be valid JSON");
    assert!(value.get("trashed").is_some(), "{value}");
    assert!(value.get("purged").is_some(), "{value}");
    assert_eq!(value["failed"].as_array().expect("array").len(), 0);
    assert!(
        value.get("candidates").is_none(),
        "an outcome is not a plan, and must not look like one: {value}"
    );
}

/// A refusal removed nothing, so what there is to report is the plan. The two
/// are told apart by the exit code, which a consumer has to read anyway.
#[test]
fn a_refusal_emits_the_plan_and_exits_two() {
    let dir = mixed_tiers_dir();

    let output = run(&[
        "clean",
        dir.path().to_str().expect("utf8"),
        "--older-than",
        "1d",
        "--json",
    ]);

    assert_eq!(output.status.code(), Some(2), "{:?}", output.status);
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout must be valid JSON");
    assert!(
        value.get("candidates").is_some(),
        "nothing happened, so the plan is what there is to say: {value}"
    );
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

    let output = run(&["scan", dir.path().to_str().expect("utf8 path")]);

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
fn clean_without_a_path_announces_what_it_walks() {
    // The built-in `user-caches` is rooted at the home directory, so a bare
    // `clean` walks all of it. On a real machine that is minutes; nobody should
    // have to guess why.
    let output = run(&["preview"]);

    assert!(output.status.success(), "{:?}", output.status);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("examining"),
        "a walk the user did not name must say where it is going:\n{stderr}"
    );
}

/// Every rule unrooted — `root = "*"` — so nothing names a directory to walk.
/// Not an error, and not an empty plan either: "nothing to clean" is a claim
/// about the disk, and this is a statement about the configuration.
#[test]
fn a_config_that_names_no_directory_says_so() {
    let home = isolated();
    let config = write(
        home.path(),
        "config.yml",
        "clean-rules:\n  - name: \"anywhere\"\n    parts:\n      - root: \"*\"\n        includes: [\"**/x/\"]\n",
    );

    let output = run(&["--config", config.to_str().expect("utf8"), "preview"]);

    assert!(
        output.status.success(),
        "a configuration that covers nothing is not an error, got {:?}",
        output.status
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no rule names a directory"),
        "silence would read as 'nothing to clean':\n{stderr}"
    );
    assert!(
        stderr.contains("Pass a path") && stderr.contains("root"),
        "and the message must name both remedies:\n{stderr}"
    );
    assert!(
        !stderr.contains("examining"),
        "nothing was walked:\n{stderr}"
    );
}

/// The load-bearing safety property: `preview` does nothing at all.
#[test]
fn a_preview_writes_nothing() {
    let dir = cleanable_dir();
    let before = snapshot(dir.path());

    let output = run(&[
        "preview",
        dir.path().to_str().expect("utf8 path"),
        "-d",
        "1",
    ]);

    assert!(output.status.success(), "{:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("node_modules"),
        "the fixture must actually match, or this proves nothing:\n{stdout}"
    );
    assert!(
        stdout.contains("Preview — nothing was removed"),
        "and the report must say it removed nothing:\n{stdout}"
    );
    assert_eq!(
        before,
        snapshot(dir.path()),
        "a dry run must leave the tree byte-identical"
    );
}

/// `clean` really removes, end to end through the binary.
///
/// `#[ignore]` for the reason every real-trash test here carries: it puts
/// things in the developer's actual Trash. Run via `just smoke-trash`.
#[test]
#[ignore = "moves real files to the OS trash; run via `just smoke-trash`"]
fn clean_removes_the_candidates() {
    let dir = cleanable_dir();
    let path = dir.path().to_str().expect("utf8 path");

    let output = run(&["clean", path]);

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

    let output = run(&["clean", root.to_str().expect("utf8 path")]);

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

    let output = run(&["preview", dir.path().to_str().expect("utf8 path")]);

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

    let everything = run(&["preview", path, "--older-than", "90d", "-d", "1"]);
    let stdout = String::from_utf8_lossy(&everything.stdout);
    assert!(
        stdout.contains("ancient-one.bin") && stdout.contains("node_modules"),
        "the fixture must offer both tiers, or this proves nothing:\n{stdout}"
    );

    let safe = run(&["preview", path, "--older-than", "90d", "--safe", "-d", "1"]);
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

    let output = run(&["clean", path, "--purge"]);

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

/// `--purge` no longer requires a companion flag — the verb it modifies already
/// removes. On `preview` it still removes nothing, which is the property that
/// makes the flag safe to carry across from one line to the other.
#[test]
fn purge_on_a_preview_removes_nothing() {
    let dir = cleanable_dir();
    let before = snapshot(dir.path());

    let output = run(&[
        "preview",
        dir.path().to_str().expect("utf8 path"),
        "--purge",
    ]);

    assert!(output.status.success(), "{:?}", output.status);
    assert_eq!(before, snapshot(dir.path()), "nothing was removed");
}

// ---- the configuration file ---------------------------------------------
//
// Driven through `--config` so that no test here depends on — or writes to —
// the real config directory of whoever is running them.

/// A `clean` fixture with one `node_modules` in it.
fn node_modules_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir(dir.path().join("node_modules")).expect("mkdir");
    std::fs::write(dir.path().join("node_modules/lib.bin"), vec![b'x'; 4096]).expect("write");
    dir
}

fn write(dir: &Path, name: &str, text: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, text).expect("write config");
    path
}

/// The point of the whole feature: a rule the user wrote is the rule that runs.
#[test]
fn a_configured_rule_replaces_the_builtins() {
    let fixture = node_modules_dir();
    let home = tempfile::tempdir().expect("tempdir");
    let config = write(
        home.path(),
        "config.yml",
        "clean-rules:\n  - name: \"mine\"\n    parts:\n      - root: \"*\"\n        includes: [\"**/node_modules/\"]\n    tier: \"trash\"\n",
    );

    let output = run(&[
        "--config",
        config.to_str().expect("utf8"),
        "preview",
        fixture.path().to_str().expect("utf8"),
    ]);

    assert!(output.status.success(), "{:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("mine"),
        "the report names the rule that claimed it:\n{stdout}"
    );
    assert!(
        !stdout.contains("node-modules"),
        "and the built-in rule it replaced is gone:\n{stdout}"
    );
}

/// A config file is the input to a delete operation. If it cannot be
/// understood, the rules are unknown — so nothing is scanned and nothing runs.
#[test]
fn a_malformed_config_stops_the_program_before_scanning() {
    let fixture = node_modules_dir();
    let home = tempfile::tempdir().expect("tempdir");
    let config = write(
        home.path(),
        "config.yml",
        "scan:\n  one-file-system: true\n bad-indent: 1\n",
    );

    let output = run(&[
        "--config",
        config.to_str().expect("utf8"),
        "preview",
        fixture.path().to_str().expect("utf8"),
    ]);

    assert_eq!(output.status.code(), Some(2), "{:?}", output.status);
    assert!(
        output.stdout.is_empty(),
        "nothing may be reported: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("line 3"),
        "must locate the mistake:\n{stderr}"
    );
}

/// A named file that is not there is a typo, not a request for defaults.
#[test]
fn an_explicit_config_that_is_absent_is_an_error() {
    let fixture = node_modules_dir();
    let home = tempfile::tempdir().expect("tempdir");

    let output = run(&[
        "--config",
        home.path().join("nope.toml").to_str().expect("utf8"),
        "preview",
        fixture.path().to_str().expect("utf8"),
    ]);

    assert_eq!(output.status.code(), Some(2), "{:?}", output.status);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("nope.toml"),
        "the message must name the file it looked for"
    );
}

/// A typo or a newer version's key. Naming it is enough; refusing to run would
/// make the tool brittle and protect nothing.
#[test]
fn an_unknown_key_warns_but_the_run_continues() {
    let fixture = node_modules_dir();
    let home = tempfile::tempdir().expect("tempdir");
    let config = write(
        home.path(),
        "config.yml",
        "scan:\n  one-file-sistem: true\n",
    );

    let output = run(&[
        "--config",
        config.to_str().expect("utf8"),
        "preview",
        fixture.path().to_str().expect("utf8"),
    ]);

    assert!(output.status.success(), "{:?}", output.status);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("one-file-sistem"),
        "the typo must be findable:\n{stderr}"
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("node-modules"),
        "and the built-in rules still ran"
    );
}

/// A rule that cannot be understood is refused by name — an error about "a
/// rule" in a file with twelve of them is not one a user can act on.
#[test]
fn a_rule_missing_its_root_is_refused_by_name() {
    let fixture = node_modules_dir();
    let home = tempfile::tempdir().expect("tempdir");
    let config = write(
        home.path(),
        "config.yml",
        "clean-rules:\n  - name: \"mine\"\n    parts:\n      - includes: [\"**/x/\"]\n",
    );

    let output = run(&[
        "--config",
        config.to_str().expect("utf8"),
        "preview",
        fixture.path().to_str().expect("utf8"),
    ]);

    assert_eq!(output.status.code(), Some(2), "{:?}", output.status);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("rule `mine`"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn config_init_writes_a_usable_file_and_prints_its_path() {
    let home = tempfile::tempdir().expect("tempdir");
    let target = home.path().join("nested").join("config.yml");

    let output = run(&["--config", target.to_str().expect("utf8"), "config", "init"]);

    assert!(output.status.success(), "{:?}", output.status);
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        target.to_str().expect("utf8"),
        "the path is the result, so it goes to stdout"
    );

    // And the file it wrote is one the tool can read back.
    let fixture = node_modules_dir();
    let reread = run(&[
        "--config",
        target.to_str().expect("utf8"),
        "preview",
        fixture.path().to_str().expect("utf8"),
    ]);
    assert!(reread.status.success(), "{:?}", reread.status);
    assert!(String::from_utf8_lossy(&reread.stdout).contains("node-modules"));
}

/// A config is something a user edits, and "show me the defaults" must not be
/// able to throw those edits away.
#[test]
fn config_init_refuses_to_overwrite_without_force() {
    let home = tempfile::tempdir().expect("tempdir");
    let target = write(home.path(), "config.yml", "# mine\n");

    let output = run(&["--config", target.to_str().expect("utf8"), "config", "init"]);

    assert!(!output.status.success(), "{:?}", output.status);
    assert!(String::from_utf8_lossy(&output.stderr).contains("--force"));
    assert_eq!(
        std::fs::read_to_string(&target).expect("read"),
        "# mine\n",
        "the edits are still there"
    );

    let forced = run(&[
        "--config",
        target.to_str().expect("utf8"),
        "config",
        "init",
        "--force",
    ]);
    assert!(forced.status.success(), "{:?}", forced.status);
    assert!(
        std::fs::read_to_string(&target)
            .expect("read")
            .contains("clean-rules:\n  - name:")
    );
}

/// A `node_modules` of a known size under `dir`.
fn seed_node_modules(dir: &Path, bytes: usize) {
    let nested = dir.join("node_modules");
    std::fs::create_dir_all(&nested).expect("mkdir");
    std::fs::write(nested.join("lib.bin"), vec![b'x'; bytes]).expect("write");
}

/// A config rooting one rule at each of `roots`.
fn rules_rooted_at(at: &Path, roots: &[&Path]) -> std::path::PathBuf {
    let mut text = String::from("clean-rules:\n");
    for (index, root) in roots.iter().enumerate() {
        text.push_str(&format!(
            "  - name: \"r{index}\"\n    tier: \"trash\"\n    parts:\n      - root: {:?}\n        includes: [\"**/node_modules/\"]\n",
            root.to_str().expect("utf8")
        ));
    }
    write(at, "config.yml", &text)
}

/// The promise Task 2 made when it required `root` in the file.
#[test]
fn two_rule_roots_are_both_walked() {
    let home = isolated();
    let a = home.path().join("a");
    let b = home.path().join("b");
    seed_node_modules(&a, 4096);
    seed_node_modules(&b, 4096);
    let config = rules_rooted_at(home.path(), &[&a, &b]);

    let output = run(&[
        "--config",
        config.to_str().expect("utf8"),
        "preview",
        "-d",
        "1",
    ]);

    assert!(output.status.success(), "{:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(a.join("node_modules").to_str().expect("utf8")),
        "{stdout}"
    );
    assert!(
        stdout.contains(b.join("node_modules").to_str().expect("utf8")),
        "{stdout}"
    );
}

/// A root inside another must not be walked twice.
///
/// Asserted on the **total**, not on the number of lines: a duplicated candidate
/// is what doubles `reclaimable`, and that figure is what a user reads to decide
/// whether the cleanup is worth doing.
#[test]
fn a_nested_root_does_not_double_the_total() {
    let home = isolated();
    let outer = home.path().join("outer");
    let inner = outer.join("inner");
    seed_node_modules(&inner, 100_000);

    let both = rules_rooted_at(home.path(), &[&outer, &inner]);
    let just_outer = {
        let dir = isolated();
        let path = rules_rooted_at(dir.path(), &[&outer]);
        std::fs::read_to_string(&path).expect("read")
    };
    let only = write(home.path(), "only.toml", &just_outer);

    let with_both = run(&["--config", both.to_str().expect("utf8"), "preview"]);
    let with_one = run(&["--config", only.to_str().expect("utf8"), "preview"]);

    assert!(with_both.status.success() && with_one.status.success());
    assert_eq!(
        String::from_utf8_lossy(&with_both.stdout),
        String::from_utf8_lossy(&with_one.stdout),
        "the nested root adds nothing, so the report and the total are unchanged"
    );
}

/// A rule's root is a description, and descriptions go stale. One directory that
/// has moved is no reason to leave the others uncleaned.
#[test]
fn a_rule_root_that_is_gone_is_skipped_not_fatal() {
    let home = isolated();
    let real = home.path().join("real");
    seed_node_modules(&real, 4096);
    let vanished = home.path().join("vanished");
    let config = rules_rooted_at(home.path(), &[&vanished, &real]);

    let output = run(&[
        "--config",
        config.to_str().expect("utf8"),
        "preview",
        "-d",
        "1",
    ]);

    assert!(
        output.status.success(),
        "a stale root must not stop the run, got {:?}",
        output.status
    );
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains(real.join("node_modules").to_str().expect("utf8")),
        "the root that is still there was cleaned"
    );
}

/// The other half of that decision: a path the **user** named and that is not
/// there is a typo, and an empty report would hide it.
#[test]
fn a_named_path_that_is_gone_is_still_an_error() {
    let home = isolated();

    let output = run(&["clean", home.path().join("nope").to_str().expect("utf8")]);

    assert_eq!(output.status.code(), Some(2), "{:?}", output.status);
    assert!(String::from_utf8_lossy(&output.stderr).contains("nope"));
}

// ---- confirmation --------------------------------------------------------
//
// The concept asks that a non-regenerable candidate never go without explicit
// consent. v0.3 gives that as a refusal rather than a prompt, which is why every
// test below is an ordinary one: refusing deletes nothing, so the headline
// property is checkable in a normal run rather than behind `just smoke-trash`.
//
// The three that do get past the refusal pass `--purge`, and not for speed. The
// refusal is decided before the removal method matters, so either flag exercises
// the same branch — but `clean` alone would put a temp fixture into the real
// Trash of whoever ran the suite, which is exactly what this project keeps
// behind `#[ignore]`. `--purge` confines the deletion to the temp directory.
// (It is also 70x faster: measured at 121 s for these three against the trash.)

/// A `node_modules` (auto tier) beside something only the age rule can claim,
/// which is confirm tier.
fn mixed_tiers_dir() -> tempfile::TempDir {
    let dir = cleanable_dir();
    let stale = dir.path().join("notes.txt");
    std::fs::write(&stale, b"old").expect("write");
    // 2001-01-01, comfortably past any threshold a test will use.
    let long_ago = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(978_307_200);
    filetime_set(&stale, long_ago);
    assert_eq!(
        std::fs::metadata(&stale).expect("stat").modified().ok(),
        Some(long_ago),
        "every confirm-tier test below rests on this file being old"
    );
    dir
}

/// Backdate a file, so an `--older-than` threshold has something to find.
///
/// `File::set_times` rather than shelling out to `touch`. The subprocess version
/// tried the GNU spelling, printed its failure to the suite's stderr on every
/// BSD run, and fell back to a **hardcoded** date that merely happened to equal
/// the one caller's argument. Worse, it ignored both exit statuses: on a
/// platform where neither form worked, the file would keep its current mtime and
/// the age tests would pass without testing anything.
fn filetime_set(path: &Path, when: std::time::SystemTime) {
    let file = std::fs::File::options()
        .write(true)
        .open(path)
        .expect("open for its timestamps");
    file.set_times(std::fs::FileTimes::new().set_modified(when))
        .expect("set mtime");
}

/// The headline property: `clean` on its own does not take what cannot be
/// regenerated.
#[test]
fn clean_refuses_while_a_confirm_tier_candidate_remains() {
    let dir = mixed_tiers_dir();
    let before = snapshot(dir.path());

    let output = run(&[
        "clean",
        dir.path().to_str().expect("utf8"),
        "--older-than",
        "1d",
    ]);

    assert_eq!(
        output.status.code(),
        Some(2),
        "an incomplete invitation is a usage answer, got {:?}",
        output.status
    );
    assert_eq!(
        before,
        snapshot(dir.path()),
        "and absolutely nothing was removed"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not regenerable") && stderr.contains("nothing was removed"),
        "{stderr}"
    );
    assert!(
        stderr.contains("--safe") && stderr.contains("--yes"),
        "both remedies must be named:\n{stderr}"
    );
}

/// The refusal prints the plan as a **preview**. Printing "about to remove" and
/// then declining would make the last thing a user reads before the outcome the
/// one sentence in the report that is false — the defect `Intent` exists for.
#[test]
fn the_refusal_does_not_promise_a_removal() {
    let dir = mixed_tiers_dir();

    let output = run(&[
        "clean",
        dir.path().to_str().expect("utf8"),
        "--older-than",
        "1d",
    ]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Preview — nothing was removed"),
        "the plan must close as a preview, since that is what happened:\n{stdout}"
    );
}

/// A plan of only regenerable candidates needs no confirmation, so `clean`
/// proceeds — which is what makes a bare `clean` usable at all.
#[test]
fn clean_proceeds_when_nothing_needs_confirming() {
    let dir = cleanable_dir();

    let output = run(&["clean", dir.path().to_str().expect("utf8"), "--purge"]);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("Add --safe"),
        "an auto-tier plan must not be refused:\n{stderr}"
    );
}

/// `--safe` keeps confirm-tier candidates out of the plan, so the count is zero
/// and there is nothing to refuse. Pinned rather than left a coincidence.
#[test]
fn safe_removes_the_regenerable_ones_without_a_refusal() {
    let dir = mixed_tiers_dir();

    let output = run(&[
        "clean",
        dir.path().to_str().expect("utf8"),
        "--older-than",
        "1d",
        "--safe",
        "--purge",
    ]);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("Add --safe"),
        "--safe already answered the question:\n{stderr}"
    );
}

/// Turning the setting off restores v0.2's behaviour: the confirmation is having
/// read the list and ran `clean`.
#[test]
fn the_setting_can_be_turned_off() {
    let dir = mixed_tiers_dir();
    let home = isolated();
    let config = write(
        home.path(),
        "config.yml",
        "clean:\n  require-confirmation: false\n",
    );

    let output = run(&[
        "--config",
        config.to_str().expect("utf8"),
        "clean",
        dir.path().to_str().expect("utf8"),
        "--older-than",
        "1d",
        "--purge",
    ]);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("Add --safe"),
        "the file said not to ask:\n{stderr}"
    );
    assert!(
        stderr.contains("removing anyway, as asked"),
        "but the count is still said out loud:\n{stderr}"
    );
}

/// `--yes` on a preview is accepted and does nothing, which is the point:
/// the way a preview is acted on is to retype the line with the other verb, and
/// a flag one of them rejected would break the copy exactly then.
#[test]
fn yes_on_a_preview_is_accepted_and_removes_nothing() {
    let dir = cleanable_dir();
    let before = snapshot(dir.path());

    let output = run(&["preview", dir.path().to_str().expect("utf8"), "--yes"]);

    assert!(output.status.success(), "{:?}", output.status);
    assert_eq!(before, snapshot(dir.path()));
}

/// Every flag either verb takes, on a preview, still changes nothing. The one
/// promise `preview` makes, asserted against the flags most able to break it.
#[test]
fn a_preview_changes_nothing_whatever_the_flags() {
    let dir = cleanable_dir();
    let path = dir.path().to_str().expect("utf8");
    let before = snapshot(dir.path());

    for extra in [
        vec![],
        vec!["--purge"],
        vec!["--yes"],
        vec!["--safe"],
        vec!["--purge", "--yes"],
        vec!["--older-than", "1d"],
        vec!["--min-size", "1"],
    ] {
        let mut args = vec!["preview", path];
        args.extend_from_slice(&extra);
        let output = run(&args);

        assert!(output.status.success(), "{extra:?}: {:?}", output.status);
        assert_eq!(before, snapshot(dir.path()), "{extra:?} removed something");
    }
}

/// Both are gone from the surface. A flag that no longer exists must be a usage
/// error that names itself — and, above all, must not remove anything on its way
/// to being refused.
#[test]
fn the_removed_flags_are_usage_errors_that_change_nothing() {
    let dir = cleanable_dir();
    let before = snapshot(dir.path());

    for flag in ["--apply", "--allow-dirty"] {
        let output = run(&["clean", dir.path().to_str().expect("utf8"), flag]);

        assert_eq!(output.status.code(), Some(2), "{flag}: {:?}", output.status);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(flag), "the error must name it:\n{stderr}");
        assert_eq!(before, snapshot(dir.path()), "{flag} removed something");
    }
}

/// The count is said aloud only when there is something to say.
///
/// `confirm > 0` mutated to `>=` survived every test: with `--yes` on a plan of
/// only regenerable candidates the tool would have announced "0 of these are not
/// regenerable — removing anyway", which is both false and alarming.
#[test]
fn nothing_is_announced_when_nothing_needs_confirming() {
    let dir = cleanable_dir();

    let output = run(&[
        "clean",
        dir.path().to_str().expect("utf8"),
        "--purge",
        "--yes",
    ]);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("not regenerable"),
        "an all-auto plan has nothing to say about confirmation:\n{stderr}"
    );
}

/// The refusal counts in words as well as digits. One candidate is "1 candidate
/// is", not "1 candidates are".
#[test]
fn one_refused_candidate_reads_singular() {
    let dir = mixed_tiers_dir();

    let output = run(&[
        "clean",
        dir.path().to_str().expect("utf8"),
        "--older-than",
        "1d",
    ]);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("1 candidate is not regenerable"),
        "the verb agrees with the noun, not only the noun with the count:\n{stderr}"
    );
}

// ---- the browser ---------------------------------------------------------
//
// Every test here runs with stdout piped, which is itself the point: `ui`
// refuses rather than writing escape sequences somewhere no one can read them.
// Anything needing a real terminal cannot be tested on CI and is checked by hand.

#[test]
fn ui_refuses_a_pipe_rather_than_filling_it_with_escapes() {
    let dir = isolated();

    let output = run(&["ui", dir.path().to_str().expect("utf8")]);

    assert_eq!(output.status.code(), Some(2), "{:?}", output.status);
    assert!(
        output.stdout.is_empty(),
        "not one byte may reach the pipe: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("needs a terminal"), "{stderr}");
    assert!(
        !stderr.contains('\x1b'),
        "and the refusal itself emits no escapes: {stderr:?}"
    );
}

/// A path the user named and that is not there is a typo, and it must be said
/// before the screen opens — afterwards, leaving the screen erases it.
#[test]
fn ui_reports_a_missing_path_ahead_of_the_terminal_check() {
    let dir = isolated();

    let output = run(&["ui", dir.path().join("nope").to_str().expect("utf8")]);

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("nope"), "{stderr}");
    assert!(
        !stderr.contains("needs a terminal"),
        "the path is the more useful thing to hear about: {stderr}"
    );
}

/// Unlike `scan`, `ui` opens the working directory when given none — the ban on
/// defaulting to it exists against accidentally *walking* something huge, and a
/// browser lists one directory.
#[test]
fn ui_without_a_path_is_not_a_usage_error() {
    let output = run(&["ui"]);

    // Still refused here, because these tests pipe stdout — but refused for the
    // terminal, not for a missing argument.
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("needs a terminal"),
        "a missing path must not be what stops it: {stderr}"
    );
}

#[test]
fn help_lists_the_ui_verb() {
    let output = run(&["--help"]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("ui"), "{stdout}");
}

// ---- duplicates -----------------------------------------------------------

/// End to end, on real files: the whole path from `--dup` to a report that names
/// what stays. Every unit below it is tested on hand-built values, and this is
/// the only test that proves the hashing, the plan and the report agree.
#[test]
fn preview_dup_finds_identical_files_and_says_which_one_stays() {
    let home = isolated();
    let root = home.path().join("files");
    std::fs::create_dir(&root).expect("mkdir");
    let bytes = vec![b'x'; 2 * 1024 * 1024];
    // Written first, so the default rule keeps it; sorts last, so the *path*
    // cannot be what decided.
    std::fs::write(root.join("z-original.bin"), &bytes).expect("write");
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(root.join("a-copy.bin"), &bytes).expect("write");
    std::fs::write(root.join("unique.bin"), vec![b'y'; 2 * 1024 * 1024]).expect("write");

    let output = spawn(
        &["preview", "--dup", "-d", "1", root.to_str().expect("utf-8")],
        home.path(),
    )
    .output()
    .expect("spawn disk-tools");

    assert!(output.status.success());
    let report = String::from_utf8(output.stdout).expect("utf-8");
    assert!(report.contains("keep    "), "{report}");
    assert!(report.contains("remove  "), "{report}");
    assert!(
        report.contains("a-copy.bin"),
        "the copy is offered: {report}"
    );
    assert!(
        !report.contains("unique.bin"),
        "a file with no twin is not a duplicate: {report}"
    );
    assert!(report.contains("Preview — nothing was removed"), "{report}");
    // And nothing was: a preview is a preview.
    assert!(root.join("a-copy.bin").exists());
}

/// With no path the duplicate rules say where to look, exactly as the clean
/// rules do for an ordinary run. The shipped rule is unrooted, so a bare
/// `preview --dup` has nowhere to go — and says which list to edit.
#[test]
fn dup_without_a_path_names_the_list_it_consulted() {
    let output = run(&["preview", "--dup"]);

    assert!(output.status.success(), "{:?}", output.status);
    let stderr = String::from_utf8(output.stderr).expect("utf-8");
    assert!(stderr.contains("no duplicate rule"), "{stderr}");
    assert!(
        !stderr.contains("to clean"),
        "the clean rules are not what was consulted: {stderr}"
    );
}

/// And a rooted duplicate rule is walked without a path being typed.
#[test]
fn a_rooted_duplicate_rule_is_walked_with_no_path() {
    let home = isolated();
    let root = home.path().join("files");
    std::fs::create_dir(&root).expect("mkdir");
    let bytes = vec![b'x'; 2 * 1024 * 1024];
    std::fs::write(root.join("one.bin"), &bytes).expect("write");
    std::fs::write(root.join("two.bin"), &bytes).expect("write");

    let config = write(
        home.path(),
        "config.yml",
        &format!(
            "duplicate-rules:\n  - name: here\n    parts:\n      - root: {:?}\n        includes: [\"**\"]\n",
            root.to_str().expect("utf8")
        ),
    );

    let output = spawn(
        &[
            "--config",
            config.to_str().expect("utf8"),
            "preview",
            "--dup",
        ],
        home.path(),
    )
    .output()
    .expect("spawn disk-tools");

    assert!(output.status.success(), "{:?}", output.status);
    let report = String::from_utf8(output.stdout).expect("utf-8");
    assert!(report.contains("keeps"), "{report}");
    let stderr = String::from_utf8(output.stderr).expect("utf-8");
    assert!(stderr.contains("examining"), "and it says where: {stderr}");
}

/// A duplicate is confirm-tier, always. `clean --dup` without `--yes` therefore
/// refuses, removes nothing and exits 2 — v0.5's rule, inherited whole.
#[test]
fn clean_dup_refuses_without_yes_and_leaves_both_copies() {
    let home = isolated();
    let root = home.path().join("files");
    std::fs::create_dir(&root).expect("mkdir");
    let bytes = vec![b'x'; 2 * 1024 * 1024];
    std::fs::write(root.join("one.bin"), &bytes).expect("write");
    std::fs::write(root.join("two.bin"), &bytes).expect("write");

    let output = spawn(
        &["clean", "--dup", root.to_str().expect("utf-8")],
        home.path(),
    )
    .output()
    .expect("spawn disk-tools");

    assert_eq!(output.status.code(), Some(2));
    assert!(root.join("one.bin").exists(), "nothing was removed");
    assert!(root.join("two.bin").exists(), "nothing was removed");
    let stderr = String::from_utf8(output.stderr).expect("utf-8");
    assert!(stderr.contains("--yes"), "{stderr}");
}

/// `--safe` admits only what needs no confirming, and no duplicate does.
#[test]
fn safe_and_dup_together_plan_nothing() {
    let home = isolated();
    let root = home.path().join("files");
    std::fs::create_dir(&root).expect("mkdir");
    let bytes = vec![b'x'; 2 * 1024 * 1024];
    std::fs::write(root.join("one.bin"), &bytes).expect("write");
    std::fs::write(root.join("two.bin"), &bytes).expect("write");

    let output = spawn(
        &["preview", "--dup", "--safe", root.to_str().expect("utf-8")],
        home.path(),
    )
    .output()
    .expect("spawn disk-tools");

    assert!(output.status.success());
    let report = String::from_utf8(output.stdout).expect("utf-8");
    assert!(report.contains("No duplicates found."), "{report}");
    assert!(
        report.contains("--safe is hiding"),
        "and it says the flag is why: {report}"
    );
}

/// The machine-readable half, on the same fixture.
#[test]
fn dup_json_is_groups_on_stdout_and_nothing_else() {
    let home = isolated();
    let root = home.path().join("files");
    std::fs::create_dir(&root).expect("mkdir");
    let bytes = vec![b'x'; 2 * 1024 * 1024];
    std::fs::write(root.join("one.bin"), &bytes).expect("write");
    std::fs::write(root.join("two.bin"), &bytes).expect("write");

    let output = spawn(
        &["preview", "--dup", "--json", root.to_str().expect("utf-8")],
        home.path(),
    )
    .output()
    .expect("spawn disk-tools");

    assert!(output.status.success());
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout is one JSON document");
    let groups = value["groups"].as_array().expect("groups");
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0]["copies"].as_array().expect("copies").len(), 1);
    assert!(groups[0]["keeper"]["path"].is_string());
}
