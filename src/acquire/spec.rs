//! Repository spec parsing: URL / `git@host:owner/repo` / `owner/repo` / local path.
//!
//! The local-path branch is checked first and deliberately permissive: anything
//! that looks like a path (or is a directory holding a git repo) is treated as
//! local rather than guessed at as a remote shorthand.

use crate::error::{AuditError, Result};
use crate::util::text::safe;
use regex::Regex;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoRef {
    Remote(RemoteRepo),
    Local(LocalRepo),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteRepo {
    pub display: String,
    pub host: String,
    pub owner: String,
    pub name: String,
    pub clone_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalRepo {
    pub display: String,
    pub name: String,
    pub path: String,
}

impl RepoRef {
    pub fn display(&self) -> &str {
        match self {
            RepoRef::Remote(r) => &r.display,
            RepoRef::Local(l) => &l.display,
        }
    }
}

/// `[A-Za-z0-9_][A-Za-z0-9._-]*`
///
/// Python's `$` also matches immediately before a *trailing* newline, so
/// `"o/re\n"` passed there. Rust's `$` (without `multi_line`) does not. The
/// trailing `\n?` restores Python's exact accept set — a divergence here would
/// reject a spec Python accepts, or worse, name a cache directory differently.
fn seg_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[A-Za-z0-9_][A-Za-z0-9._-]*\n?$").unwrap())
}

fn scp_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^(?P<user>[A-Za-z0-9._-]+)@(?P<host>[A-Za-z0-9.-]+):(?P<path>[^:].*)$")
            .unwrap()
    })
}

fn short_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+\n?$").unwrap())
}

fn is_git_dir(p: &Path) -> bool {
    p.join(".git").exists() || (p.join("HEAD").is_file() && p.join("objects").is_dir())
}

/// Strip a URL down to `owner/repo`, dropping `/tree/main/...` and GitLab's
/// `/-/tree/...` markers.
fn clean_segments(path: &str, host: &str, spec: &str) -> Result<Vec<String>> {
    let mut p = path.trim_matches('/');
    if let Some(stripped) = p.strip_suffix(".git") {
        p = stripped;
    }
    let mut segs: Vec<String> = p
        .split('/')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    if host.ends_with("github.com") {
        segs.truncate(2);
    } else if let Some(dash) = segs.iter().position(|s| s == "-") {
        segs.truncate(dash);
    }
    if segs.len() < 2
        || !segs
            .iter()
            .all(|s| seg_re().is_match(s) && s != "." && s != "..")
    {
        return Err(AuditError::with_hint(
            format!("Unrecognized repository address: {}", safe(spec)),
            "Expected a form such as https://github.com/owner/repo or owner/repo",
        ));
    }
    Ok(segs)
}

/// The parts of `urllib.parse.urlsplit` this tool uses. Pulling in a URL crate
/// for one function is not worth the dependency; this mirrors CPython's rules
/// closely enough for `https://` and `ssh://` git URLs.
struct SplitUrl {
    scheme: String,
    netloc: String,
    path: String,
}

fn urlsplit(url: &str) -> SplitUrl {
    // CPython: a scheme is a leading letter followed by [A-Za-z0-9+-.] up to ':'.
    let is_scheme = |s: &str| {
        let mut cs = s.chars();
        cs.next().is_some_and(|c| c.is_ascii_alphabetic())
            && cs.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    };
    let mut rest = url;
    let mut scheme = String::new();
    if let Some(i) = url.find(':') {
        if i > 0 && is_scheme(&url[..i]) {
            scheme = url[..i].to_ascii_lowercase();
            rest = &url[i + 1..];
        }
    }
    if !rest.starts_with("//") {
        return SplitUrl {
            scheme,
            netloc: String::new(),
            path: rest.to_string(),
        };
    }
    let after = &rest[2..];
    let delim = after.find(['/', '?', '#']).unwrap_or(after.len());
    let netloc = after[..delim].to_string();
    // CPython splits the fragment first, then the query.
    let mut tail = &after[delim..];
    if let Some(i) = tail.find('#') {
        tail = &tail[..i];
    }
    if let Some(i) = tail.find('?') {
        tail = &tail[..i];
    }
    SplitUrl {
        scheme,
        netloc,
        path: tail.to_string(),
    }
}

/// `userinfo` before the last `@` in the netloc.
fn url_username(netloc: &str) -> Option<String> {
    let (userinfo, _) = netloc.rsplit_once('@')?;
    if userinfo.is_empty() {
        None
    } else {
        Some(userinfo.split(':').next().unwrap_or("").to_string())
    }
}

/// Lowercased host, IPv6 brackets stripped, zone id (`%25eth0`) left alone.
fn url_hostname(netloc: &str) -> String {
    let hostinfo = netloc.rsplit_once('@').map_or(netloc, |(_, h)| h);
    let hostname = if let Some(rest) = hostinfo.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else {
        match hostinfo.find(':') {
            Some(i) => &hostinfo[..i],
            None => hostinfo,
        }
    };
    // A scoped IPv6 zone is case-sensitive; only the address part is lowercased.
    match hostname.split_once('%') {
        Some((h, z)) => format!("{}%{}", h.to_lowercase(), z),
        None => hostname.to_lowercase(),
    }
}

fn url_port(netloc: &str) -> Result<Option<u32>> {
    let hostinfo = netloc.rsplit_once('@').map_or(netloc, |(_, h)| h);
    let port_str = if hostinfo.starts_with('[') {
        hostinfo
            .split(']')
            .nth(1)
            .and_then(|s| s.strip_prefix(':'))
            .unwrap_or("")
    } else {
        match hostinfo.rfind(':') {
            Some(i) => &hostinfo[i + 1..],
            None => "",
        }
    };
    if port_str.is_empty() {
        return Ok(None);
    }
    if !port_str.chars().all(|c| c.is_ascii_digit()) {
        return Err(AuditError::with_hint(
            format!("URL port is not numeric: {}", safe(port_str)),
            "Expected a form such as https://host:port/owner/repo",
        ));
    }
    let n: u32 = port_str.parse().map_err(|_| {
        AuditError::with_hint(
            format!("URL port out of range: {}", safe(port_str)),
            "Port must be between 0 and 65535",
        )
    })?;
    if n > 65535 {
        return Err(AuditError::with_hint(
            format!("URL port out of range: {n}"),
            "Port must be between 0 and 65535",
        ));
    }
    Ok(Some(n))
}

fn expanduser(p: &str) -> PathBuf {
    if p == "~" {
        return home_dir();
    }
    if let Some(rest) = p.strip_prefix("~/") {
        return home_dir().join(rest);
    }
    PathBuf::from(p)
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

pub fn parse_repo_spec(spec: &str) -> Result<RepoRef> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err(AuditError::new("No repository address provided"));
    }
    let has_scheme = spec.contains("://");
    let is_scp = !has_scheme && scp_re().is_match(spec);

    if !has_scheme && !is_scp {
        let looks_path = spec.starts_with('/')
            || spec.starts_with("./")
            || spec.starts_with("../")
            || spec.starts_with('~')
            || spec == "."
            || spec == "..";
        let p = expanduser(spec);
        if looks_path || (p.is_dir() && is_git_dir(&p)) {
            if !p.is_dir() {
                return Err(AuditError::new(format!(
                    "Path does not exist: {}",
                    safe(spec)
                )));
            }
            let p = p.canonicalize().unwrap_or(p);
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "repo".to_string());
            return Ok(RepoRef::Local(LocalRepo {
                display: p.to_string_lossy().into_owned(),
                name,
                path: p.to_string_lossy().into_owned(),
            }));
        }
    }

    let (host_dir, segs, clone_url) = if has_scheme {
        let u = urlsplit(spec);
        if u.scheme != "https" && u.scheme != "ssh" {
            return Err(AuditError::with_hint(
                format!("Unsupported protocol: {}://", safe(&u.scheme)),
                "Only https and ssh are allowed (file://, ext::, and unencrypted http/git are rejected for security); pass local repository paths directly",
            ));
        }
        let host = url_hostname(&u.netloc);
        if host.is_empty() {
            return Err(AuditError::new(format!(
                "Could not determine hostname: {}",
                safe(spec)
            )));
        }
        let segs = clean_segments(&u.path, &host, spec)?;
        let port = match url_port(&u.netloc)? {
            Some(p) if p != 0 => format!(":{p}"),
            _ => String::new(),
        };
        // Any userinfo in the URL is discarded — credentials belong to git's
        // credential helper, not to a string we then hand to `git clone`.
        let url = if u.scheme == "https" {
            format!("https://{host}{port}/{}.git", segs.join("/"))
        } else {
            let user = url_username(&u.netloc).filter(|s| !s.is_empty());
            match user {
                Some(usr) => format!("ssh://{usr}@{host}{port}/{}.git", segs.join("/")),
                None => format!("ssh://{host}{port}/{}.git", segs.join("/")),
            }
        };
        (format!("{host}{port}"), segs, url)
    } else if is_scp {
        let c = scp_re().captures(spec).expect("scp_re matched");
        let user = c.name("user").unwrap().as_str().to_string();
        let host = c.name("host").unwrap().as_str().to_lowercase();
        let path = c.name("path").unwrap().as_str();
        let segs = clean_segments(path, &host, spec)?;
        let url = format!("{user}@{host}:{}.git", segs.join("/"));
        (host, segs, url)
    } else if short_re().is_match(spec) {
        let segs = clean_segments(spec, "github.com", spec)?;
        let url = format!("https://github.com/{}.git", segs.join("/"));
        ("github.com".to_string(), segs, url)
    } else {
        return Err(AuditError::with_hint(
            format!("Unrecognized repository address: {}", safe(spec)),
            "Supported forms: URL, git@host:owner/repo, owner/repo, or a local path",
        ));
    };

    let display = host_dir.replace('_', ":");
    Ok(RepoRef::Remote(RemoteRepo {
        display: format!("{display}/{}", segs.join("/")),
        host: host_dir.replace(':', "_"),
        owner: segs[..segs.len() - 1].join("/"),
        name: segs[segs.len() - 1].clone(),
        clone_url,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote(spec: &str) -> RemoteRepo {
        match parse_repo_spec(spec).unwrap_or_else(|e| panic!("{spec}: {e}")) {
            RepoRef::Remote(r) => r,
            _ => panic!("{spec} should be remote"),
        }
    }

    #[test]
    fn accepts_every_remote_form() {
        for spec in [
            "https://github.com/o/r",
            "https://github.com/o/r.git",
            "https://github.com/o/r/tree/main/x",
            "git@github.com:o/r.git",
            "o/r",
            "ssh://git@github.com/o/r.git",
            "https://user:tok@github.com/o/r/",
        ] {
            let r = remote(spec);
            assert_eq!(r.host, "github.com", "{spec}");
            assert_eq!(r.owner, "o", "{spec}");
            assert_eq!(r.name, "r", "{spec}");
        }
    }

    #[test]
    fn credentials_are_dropped_from_the_clone_url() {
        let r = remote("https://user:tok@github.com/o/r");
        assert!(!r.clone_url.contains("tok"), "{}", r.clone_url);
        assert_eq!(r.clone_url, "https://github.com/o/r.git");
    }

    #[test]
    fn gitlab_dash_segment_is_stripped() {
        let r = remote("https://gitlab.com/g/sub/r/-/tree/main");
        assert_eq!(r.owner, "g/sub");
        assert_eq!(r.name, "r");
    }

    #[test]
    fn rejects_unsafe_and_malformed_specs() {
        for spec in [
            "file:///etc",
            "ext::sh -c id",
            "http://github.com/o/r",
            "https://github.com/../x",
            "",
            "justname",
            "https://github.com/o/r;rm -rf",
        ] {
            assert!(parse_repo_spec(spec).is_err(), "expected rejection: {spec}");
        }
    }

    #[test]
    fn urlsplit_matches_cpython_on_the_shapes_we_accept() {
        assert_eq!(urlsplit("https://github.com/o/r").scheme, "https");
        assert_eq!(urlsplit("https://github.com/o/r").path, "/o/r");
        assert_eq!(urlsplit("ext::sh -c id").scheme, "ext");
        assert_eq!(urlsplit("ext::sh -c id").path, ":sh -c id");
        assert_eq!(urlsplit("ssh://git@host:2222/o/r").netloc, "git@host:2222");
        // Query and fragment are cut in CPython's order, leaving the path intact.
        assert_eq!(urlsplit("https://h/o/r?a=1#f").path, "/o/r");
    }

    #[test]
    fn host_and_port_extraction() {
        assert_eq!(url_hostname("GitHub.com"), "github.com");
        assert_eq!(url_hostname("user@GitHub.com:8443"), "github.com");
        assert_eq!(url_port("github.com").unwrap(), None);
        assert_eq!(url_port("github.com:8443").unwrap(), Some(8443));
        assert_eq!(url_port("github.com:0").unwrap(), Some(0));
        assert!(url_port("github.com:abc").is_err());
        assert_eq!(url_hostname("[::1]"), "::1");
    }

    #[test]
    fn ssh_url_keeps_the_user() {
        assert_eq!(
            remote("ssh://git@github.com/o/r.git").clone_url,
            "ssh://git@github.com/o/r.git"
        );
    }

    #[test]
    fn local_path_is_detected_and_named() {
        let dir = std::env::temp_dir().join("repo-audit-spec-test");
        std::fs::create_dir_all(&dir).unwrap();
        match parse_repo_spec(dir.to_str().unwrap()).unwrap() {
            RepoRef::Local(l) => assert_eq!(l.name, "repo-audit-spec-test"),
            _ => panic!("expected local"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_local_path_is_an_error() {
        assert!(parse_repo_spec("/definitely/not/here/at/all").is_err());
    }
}
