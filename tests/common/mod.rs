//! Helpers shared by integration test binaries. Not every binary uses every
//! helper, so unused items are expected here.
#![allow(dead_code)]

use std::ffi::OsString;
use std::path::PathBuf;

/// Loopback fixtures must never be reached through a proxy configured in the
/// developer's environment. reqwest (and deno_fetch) resolve NO_PROXY when the
/// client is built, so callers install this before the first run.
pub const LOOPBACK_NO_PROXY: &str = "127.0.0.1,localhost";

/// Saves process-global env vars and restores them on drop. The caller must
/// hold its test binary's env lock while the guard is alive.
pub struct EnvGuard(Vec<(&'static str, Option<OsString>)>);

impl EnvGuard {
    pub fn new(names: &[&'static str]) -> Self {
        Self(
            names
                .iter()
                .map(|&name| (name, std::env::var_os(name)))
                .collect(),
        )
    }

    /// Points DENO_DIR at `path` and bypasses proxies for loopback fetches.
    pub fn isolated_deno_dir(path: &std::path::Path) -> Self {
        let guard = Self::new(&["DENO_DIR", "NO_PROXY"]);
        std::env::set_var("DENO_DIR", path);
        std::env::set_var("NO_PROXY", LOOPBACK_NO_PROXY);
        guard
    }

    /// Bypasses proxies for loopback fetches for the guard's lifetime.
    pub fn loopback_no_proxy() -> Self {
        let guard = Self::new(&["NO_PROXY"]);
        std::env::set_var("NO_PROXY", LOOPBACK_NO_PROXY);
        guard
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (name, value) in &self.0 {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}

/// A unique temp directory removed on drop.
pub struct Fixture(pub PathBuf);

impl Fixture {
    pub fn new(prefix: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "libdeno-{prefix}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
