//! Looking up what the core refuses to.
//!
//! `disk_tools_core::ScanOptions` states the rule: the core reads no config and
//! consults no environment, so whatever the user's setup implies has already
//! been resolved by the caller. That makes every rule testable with a temporary
//! directory standing in for a home — and it makes this module the one place
//! that knows where a home actually is.
//!
//! Without it `user-caches` matches nothing and two denylist entries go
//! unenforced, so this is not plumbing: it is half of two safety rules.

use disk_tools_core::UserDirs;
use std::ffi::OsString;
use std::path::PathBuf;

/// Where this user's directories are, as far as the environment says.
///
/// `std::env::home_dir` is deliberately not used: it is deprecated at this
/// project's MSRV and documented as behaving unexpectedly on Windows, which is
/// exactly the platform the last two of these exist for.
pub fn user_dirs() -> UserDirs {
    UserDirs {
        home: home(),
        local_app_data: read("LOCALAPPDATA"),
        app_data: read("APPDATA"),
    }
}

/// `$XDG_CONFIG_HOME`, if this environment sets one.
///
/// Consulted on **every** platform, not only where XDG is the convention: a user
/// who exports it has said where their configuration lives, and ignoring that on
/// Windows because the platform has its own habit would be overruling them. The
/// platform path is the fallback, not the rule.
pub fn xdg_config_home() -> Option<PathBuf> {
    read("XDG_CONFIG_HOME")
}

/// `%USERPROFILE%` first on Windows, and the order is not arbitrary.
///
/// Git Bash, MSYS2 and Cygwin all set `HOME` to a POSIX path like
/// `/c/Users/alex`. Windows does not consider that absolute and nothing the
/// scan walked will ever equal it, so preferring `HOME` there would quietly
/// point the user-cache rules at a directory that does not exist. `USERPROFILE`
/// is set natively and is always a real Windows path.
#[cfg(windows)]
fn home() -> Option<PathBuf> {
    read("USERPROFILE").or_else(|| read("HOME"))
}

#[cfg(not(windows))]
fn home() -> Option<PathBuf> {
    read("HOME")
}

/// One variable as a path, or `None` when it is unset **or empty**.
fn read(name: &str) -> Option<PathBuf> {
    as_path(std::env::var_os(name))
}

/// The decision `read` makes once it has a value, separated from the lookup so
/// it can be tested without touching the process environment.
///
/// The empty case matters more than it looks: `PathBuf::from("")` joined with
/// `.cache` gives a *relative* `.cache`, which would then be compared against
/// absolute scan paths — matching nothing at best, and at worst turning a
/// denylist entry into something that no longer names what it was meant to
/// protect. Absent and empty mean the same thing here: we do not know.
fn as_path(value: Option<OsString>) -> Option<PathBuf> {
    value.filter(|value| !value.is_empty()).map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drives the **production** `as_path`, not a copy of it: `std::env::set_var`
    /// is `unsafe` in edition 2024 and would race every other test in this
    /// binary, so the lookup is split from the decision and the value fed in.
    #[test]
    fn an_empty_environment_variable_is_not_a_home() {
        assert_eq!(as_path(None), None, "unset means unknown");
        assert_eq!(as_path(Some(OsString::new())), None, "and so does empty");
        assert_eq!(
            as_path(Some(OsString::from("/home/me"))),
            Some(PathBuf::from("/home/me")),
            "a real value comes through unchanged"
        );
    }

    /// `user_dirs` must actually consult the environment rather than hand back
    /// an empty struct. Skips loudly where the variable it depends on is unset,
    /// since a vacuous pass would be worse than no test.
    #[test]
    fn user_dirs_reflects_the_environment() {
        let expected = if cfg!(windows) {
            std::env::var_os("USERPROFILE")
        } else {
            std::env::var_os("HOME")
        };
        let Some(expected) = expected.filter(|value| !value.is_empty()) else {
            eprintln!("skipping: this environment sets no home variable");
            return;
        };

        assert_eq!(
            user_dirs().home,
            Some(PathBuf::from(expected)),
            "the home must come from the environment, not a default"
        );
    }

    /// Whatever this machine's environment holds, the result must never contain
    /// a relative path: every consumer compares it against absolute scan paths.
    #[test]
    fn resolved_directories_are_absolute_or_absent() {
        let dirs = user_dirs();

        for (name, value) in [
            ("home", &dirs.home),
            ("local_app_data", &dirs.local_app_data),
            ("app_data", &dirs.app_data),
        ] {
            if let Some(path) = value {
                assert!(
                    path.is_absolute(),
                    "{name} resolved to a relative path: {path:?}"
                );
            }
        }
    }
}
