//! Cache and config directory resolution.

use std::path::PathBuf;

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn expanduser(p: &str) -> PathBuf {
    if p == "~" {
        return home();
    }
    if let Some(rest) = p.strip_prefix("~/") {
        return home().join(rest);
    }
    PathBuf::from(p)
}

/// Where bare clones and `.audit.db` files live.
/// `REPO_AUDIT_CACHE` overrides everything — which is also how the Rust build
/// gets a cache directory of its own while it is being diffed against Python's.
pub fn default_cache_dir() -> PathBuf {
    if let Some(env) = std::env::var_os("REPO_AUDIT_CACHE") {
        let s = env.to_string_lossy().into_owned();
        if !s.is_empty() {
            return expanduser(&s);
        }
    }
    if cfg!(target_os = "macos") {
        return home().join("Library").join("Caches").join("repo-audit");
    }
    let base = std::env::var("XDG_CACHE_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| home().join(".cache").to_string_lossy().into_owned());
    PathBuf::from(base).join("repo-audit")
}

pub fn default_config_dir() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| home().join(".config").to_string_lossy().into_owned());
    PathBuf::from(base).join("repo-audit")
}

/// Cache file name for a local repo: `<sha1(path)[..10]>-<name>.audit.db`.
/// The bare repo itself is never copied — a local path is read in place.
pub fn local_db_name(abs_path: &str, repo_name: &str) -> String {
    format!("{}-{}.audit.db", sha1_10(abs_path), repo_name)
}

fn sha1_10(s: &str) -> String {
    use sha1::{Digest, Sha1};
    let d = Sha1::digest(s.as_bytes());
    d.iter().map(|b| format!("{b:02x}")).collect::<String>()[..10].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_dir_follows_the_env_override() {
        std::env::set_var("REPO_AUDIT_CACHE", "/tmp/ra-test");
        assert_eq!(default_cache_dir(), PathBuf::from("/tmp/ra-test"));
        std::env::remove_var("REPO_AUDIT_CACHE");
        assert!(default_cache_dir().ends_with("repo-audit"));
    }

    #[test]
    fn local_db_name_matches_the_existing_cache_file() {
        // Cross-checked against the real cache on disk:
        // ~/Library/Caches/repo-audit/local/2c29f53412-Xray-core.audit.db
        let n = local_db_name("/Users/lu/repo_audit/proxyServer/Xray-core", "Xray-core");
        assert_eq!(n, "2c29f53412-Xray-core.audit.db");
        // Same input -> same name (this is what makes the cache reusable).
        assert_eq!(
            n,
            local_db_name("/Users/lu/repo_audit/proxyServer/Xray-core", "Xray-core")
        );
        assert_ne!(n, local_db_name("/somewhere/else", "Xray-core"));
    }
}
