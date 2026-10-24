//! Snapshot of the host process environment.
//!
//! [`Config::load`](crate::config::Config::load) is the config-read path for
//! sessionmunch's own settings. Everything else the CLI needs from the process
//! environment describes the *host* rather than sessionmunch: where each agent
//! harness keeps its config (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `KIRO_HOME`,
//! ...), the operator's home, `PATH`, and the host facts the Docker wrapper
//! forwards into the container (`SESSIONMUNCH_HOOK_PLATFORM`,
//! `SESSIONMUNCH_HOOKS_HOST_ROOT`). Those are captured here, once, at the
//! entry point and passed down explicitly, so no command reads the
//! environment behind its caller's back and tests can supply their own.
//!
//! It is a full snapshot rather than a fixed list because some lookups are
//! by a name only known at run time (a `${VAR}` placeholder in a Grok config).

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

/// The process environment as it was when the CLI started.
///
/// Lookups behave like `std::env::var_os` / `std::env::var(..).ok()`,
/// including Windows' case-insensitive variable names. Nothing in this
/// workspace mutates its own environment (`set_var` is `unsafe` under
/// edition 2024 and `unsafe_code` is forbidden), so a snapshot taken at
/// startup reads the same values a lazy read would have.
#[derive(Default)]
pub struct HostEnv {
    vars: HashMap<OsString, OsString>,
}

impl HostEnv {
    /// Capture the current process environment. Called once, by the entry point.
    #[must_use]
    // The one sanctioned host-environment reader outside `Config::load`.
    #[allow(clippy::disallowed_methods)]
    pub fn capture() -> Self {
        Self::from_vars(std::env::vars_os())
    }

    /// Build a snapshot from explicit pairs.
    pub(crate) fn from_vars<K, V>(vars: impl IntoIterator<Item = (K, V)>) -> Self
    where
        K: Into<OsString>,
        V: Into<OsString>,
    {
        let mut map = HashMap::new();
        for (name, value) in vars {
            // First wins, as with the OS lookup of a duplicated name.
            map.entry(normalize_name(&name.into()))
                .or_insert_with(|| value.into());
        }
        Self { vars: map }
    }

    /// `std::env::var_os(name)` as of startup.
    #[must_use]
    pub fn var_os(&self, name: &str) -> Option<OsString> {
        self.vars.get(&normalize_name(OsStr::new(name))).cloned()
    }

    /// `std::env::var(name).ok()` as of startup: `None` when unset or not
    /// valid Unicode.
    #[must_use]
    pub fn var(&self, name: &str) -> Option<String> {
        self.var_os(name)?.into_string().ok()
    }

    /// The user home used for agent configuration paths.
    ///
    /// Tests and scripted wrappers can set `SESSIONMUNCH_HOME` to exercise the
    /// platform-specific config layout without touching the real user profile.
    #[must_use]
    pub fn home_dir(&self) -> Option<PathBuf> {
        if let Some(home) = self
            .var_os("SESSIONMUNCH_HOME")
            .filter(|value| !value.is_empty())
        {
            return Some(PathBuf::from(home));
        }
        dirs::home_dir()
    }
}

/// Windows treats variable names case-insensitively (`Path` answers a lookup
/// for `PATH`), so names are folded there; everywhere else they are exact.
#[cfg(windows)]
fn normalize_name(name: &OsStr) -> OsString {
    name.to_ascii_uppercase()
}

#[cfg(not(windows))]
fn normalize_name(name: &OsStr) -> OsString {
    name.to_os_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookups_mirror_the_std_accessors() {
        let env = HostEnv::from_vars([("SET", "value"), ("EMPTY", "")]);
        assert_eq!(env.var_os("SET"), Some(OsString::from("value")));
        assert_eq!(env.var("SET").as_deref(), Some("value"));
        assert_eq!(env.var("EMPTY").as_deref(), Some(""));
        assert_eq!(env.var_os("UNSET"), None);
        assert_eq!(env.var("UNSET"), None);
    }

    #[cfg(unix)]
    #[test]
    fn var_is_none_for_a_non_unicode_value_like_std_env_var() {
        use std::os::unix::ffi::OsStringExt as _;
        let env =
            HostEnv::from_vars([(OsString::from("BAD"), OsString::from_vec(vec![0x66, 0x80]))]);
        assert!(env.var_os("BAD").is_some());
        assert_eq!(env.var("BAD"), None);
    }

    #[test]
    fn names_are_case_insensitive_only_on_windows() {
        let env = HostEnv::from_vars([("Path", "/bin")]);
        assert_eq!(env.var("PATH").is_some(), cfg!(windows));
        assert_eq!(env.var("Path").as_deref(), Some("/bin"));
    }

    #[test]
    fn home_dir_prefers_a_non_empty_sessionmunch_home() {
        let env = HostEnv::from_vars([("SESSIONMUNCH_HOME", "/wrapped/home")]);
        assert_eq!(env.home_dir(), Some(PathBuf::from("/wrapped/home")));
        let blank = HostEnv::from_vars([("SESSIONMUNCH_HOME", "")]);
        assert_eq!(blank.home_dir(), dirs::home_dir());
    }
}
