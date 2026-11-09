//! Every `git` invocation goes through here.
//!
//! The audited repository is **untrusted input**. `core.hooksPath`,
//! `protocol.*`, `ext.diff` and `textconv` are all repository-controlled
//! settings that can turn a read-only command into arbitrary code execution.
//! The switches below are a red line — see AGENTS.md (Red lines). Do not "tidy" any
//! of them away for readability.

use crate::error::{AuditError, Result};
use crate::util::text::safe;
use std::ffi::OsStr;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

pub const GIT_SAFE_CFG: &[&str] = &[
    "core.hooksPath=/dev/null",
    "core.fsmonitor=false",
    "core.quotepath=false",
    "protocol.allow=never",
    "protocol.https.allow=always",
    "protocol.ssh.allow=always",
];

/// Variables that would silently redirect the command at a *different*
/// repository than the one being audited.
const STRIPPED_ENV: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
];

/// `base` is the environment to start from. Taking it as a parameter rather
/// than reading `std::env::vars_os()` keeps this unit-testable without mutating
/// process-global state (which would race every other test in the binary).
fn git_env_from<I>(base: I) -> Vec<(std::ffi::OsString, std::ffi::OsString)>
where
    I: IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
{
    let mut env: Vec<(std::ffi::OsString, std::ffi::OsString)> = base.into_iter().collect();
    env.retain(|(k, _)| {
        let s = k.to_string_lossy();
        !STRIPPED_ENV.contains(&s.as_ref())
    });
    let set = |env: &mut Vec<(std::ffi::OsString, std::ffi::OsString)>, k: &str, v: &str| {
        let ko = std::ffi::OsString::from(k);
        if let Some(slot) = env.iter_mut().find(|(ek, _)| *ek == ko) {
            slot.1 = std::ffi::OsString::from(v);
        } else {
            env.push((ko, std::ffi::OsString::from(v)));
        }
    };
    set(&mut env, "GIT_TERMINAL_PROMPT", "0");
    set(&mut env, "LC_ALL", "C");
    set(&mut env, "GIT_OPTIONAL_LOCKS", "0");
    set(&mut env, "GIT_PAGER", "cat");
    // setdefault: the user's own ssh setup is not ours to override.
    if !env.iter().any(|(k, _)| k == "GIT_SSH_COMMAND") {
        set(&mut env, "GIT_SSH_COMMAND", "ssh -o BatchMode=yes");
    }
    env
}

pub(crate) fn git_env() -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    git_env_from(std::env::vars_os())
}

fn base_cmd() -> Command {
    let mut c = Command::new("git");
    c.env_clear();
    for (k, v) in git_env() {
        c.env(k, v);
    }
    c
}

fn spawn_error() -> AuditError {
    AuditError::with_hint(
        "git not found",
        "On macOS, run: xcode-select --install or brew install git",
    )
}

fn timeout_error(timeout: Duration, head: &str) -> AuditError {
    AuditError::with_hint(
        format!(
            "git command timed out after {}s: git {}",
            timeout.as_secs(),
            head
        ),
        "Increase the limit with --timeout",
    )
}

/// Turn git's English stderr into something actionable. The empty string means
/// "no better explanation than the raw error".
pub fn explain_git_failure(stderr: &str) -> String {
    let s = stderr.to_lowercase();
    if s.contains("repository not found") || (s.contains("not found") && s.contains("repository")) {
        return "Repository not found, or the current credentials cannot access this private repository".into();
    }
    for k in [
        "authentication failed",
        "could not read username",
        "terminal prompts disabled",
        "invalid credentials",
    ] {
        if s.contains(k) {
            return "Authentication failed, or the repository does not exist or is private (GitHub also requires authentication for nonexistent repositories); for private repositories, configure a git credential helper (macOS Keychain) or run `gh auth login` first".into();
        }
    }
    if s.contains("permission denied (publickey)") {
        return "SSH public key was rejected: check ssh-agent and whether the key has been added to the platform".into();
    }
    if s.contains("could not resolve host") || s.contains("unable to access") {
        return "Network unavailable or hostname could not be resolved".into();
    }
    String::new()
}

/// What one git invocation produced.
pub struct GitOutput {
    pub ok: bool,
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl GitOutput {
    /// stderr decoded and trimmed, for messages.
    pub fn err_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).trim().to_string()
    }

    /// The last `n` characters of stderr, sanitized for terminal output.
    pub fn err_tail(&self, n: usize) -> String {
        let s: String = self.err_text();
        let cut = s.chars().count().saturating_sub(n);
        safe(&s.chars().skip(cut).collect::<String>())
    }
}

#[derive(Clone, Default)]
pub struct Git {
    pub git_dir: Option<PathBuf>,
    pub allow_file: bool,
    /// `mailmap.<file>` override, so a repo's `.mailmap` can be honoured or
    /// replaced without touching the audited working copy.
    pub mailmap_file: Option<String>,
}

impl Git {
    pub fn new(git_dir: Option<PathBuf>) -> Self {
        Self {
            git_dir,
            allow_file: false,
            mailmap_file: None,
        }
    }

    pub fn with_file(git_dir: Option<PathBuf>) -> Self {
        Self {
            git_dir,
            allow_file: true,
            mailmap_file: None,
        }
    }

    /// Full argv. Kept public so a caller can assert on it in tests — this is
    /// the single place the security switches are assembled.
    pub fn argv(&self, args: &[&OsStr], cfg: &[String]) -> Vec<std::ffi::OsString> {
        let mut a: Vec<std::ffi::OsString> = vec!["git".into()];
        if let Some(d) = &self.git_dir {
            a.push("--git-dir".into());
            a.push(d.clone().into_os_string());
        }
        for c in GIT_SAFE_CFG {
            a.push("-c".into());
            a.push((*c).into());
        }
        if self.allow_file {
            a.push("-c".into());
            a.push("protocol.file.allow=always".into());
        }
        for c in cfg {
            a.push("-c".into());
            a.push(c.clone().into());
        }
        a.extend(args.iter().map(|s| (*s).to_os_string()));
        a
    }

    /// Run to completion, capturing stdout and the raw stderr.
    ///
    /// `check` is what turns a non-zero exit into an [`AuditError`]. Probing
    /// callers (`rev-parse --verify`, `show-ref --quiet`) pass `check=false`
    /// and read the boolean from [`Git::try_run`] instead — with `check=false`
    /// alone a failure is indistinguishable from success.
    ///
    /// `inherit_stderr` lets git's own progress output through while a user is
    /// watching; otherwise stderr is captured so a failure can be explained.
    pub fn run_os(
        &self,
        args: &[&OsStr],
        cfg: &[String],
        timeout: Option<Duration>,
        check: bool,
        inherit_stderr: bool,
        input: Option<&[u8]>,
    ) -> Result<Vec<u8>> {
        let r = self.try_run(args, cfg, timeout, inherit_stderr, input)?;
        if check && !r.ok {
            let name = args.iter().filter_map(|a| a.to_str()).next().unwrap_or("?");
            let mut msg = format!("git {name} failed (exit code {:?})", r.code.unwrap_or(-1));
            let err = r.err_text();
            if !err.is_empty() {
                msg.push_str(": ");
                msg.push_str(&r.err_tail(400));
            }
            return Err(AuditError::with_hint(msg, explain_git_failure(&err)));
        }
        Ok(r.stdout)
    }

    /// Full result: success flag, stdout, stderr.
    pub fn try_run(
        &self,
        args: &[&OsStr],
        cfg: &[String],
        timeout: Option<Duration>,
        inherit_stderr: bool,
        input: Option<&[u8]>,
    ) -> Result<GitOutput> {
        let argv = self.argv(args, cfg);
        let mut cmd = base_cmd();
        // argv[0] is "git" for display and tests; Command already names it.
        cmd.args(&argv[1..])
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(if inherit_stderr {
                Stdio::inherit()
            } else {
                Stdio::piped()
            });
        let mut child = cmd.spawn().map_err(|_| spawn_error())?;
        if let (Some(data), Some(mut si)) = (input, child.stdin.take()) {
            si.write_all(data)
                .map_err(|e| AuditError::new(format!("Failed to write to git stdin: {e}")))?;
        }
        // stderr is drained on its own thread: git can emit more than a pipe
        // buffer of progress, and reading stdout to EOF first would deadlock
        // against a child still blocked writing stderr.
        let errpipe = if inherit_stderr {
            None
        } else {
            child.stderr.take().map(|mut se| {
                std::thread::spawn(move || {
                    let mut v = Vec::new();
                    let _ = se.read_to_end(&mut v);
                    v
                })
            })
        };
        let mut stdout = Vec::new();
        child
            .stdout
            .take()
            .unwrap()
            .read_to_end(&mut stdout)
            .map_err(|e| AuditError::new(format!("Failed to read git output: {e}")))?;

        let status = match timeout {
            None => child
                .wait()
                .map_err(|e| AuditError::new(format!("git subprocess error: {e}")))?,
            Some(t) => {
                let start = std::time::Instant::now();
                loop {
                    match child
                        .try_wait()
                        .map_err(|e| AuditError::new(format!("git subprocess error: {e}")))?
                    {
                        Some(s) => break s,
                        None => {
                            if start.elapsed() >= t {
                                let _ = child.kill();
                                let _ = child.wait();
                                let head = args
                                    .iter()
                                    .filter_map(|a| a.to_str())
                                    .take(2)
                                    .collect::<Vec<_>>()
                                    .join(" ");
                                return Err(timeout_error(t, &head));
                            }
                            std::thread::sleep(Duration::from_millis(5));
                        }
                    }
                }
            }
        };
        let stderr = errpipe
            .map(|h| h.join().unwrap_or_default())
            .unwrap_or_default();
        Ok(GitOutput {
            ok: status.success(),
            code: status.code(),
            stdout,
            stderr,
        })
    }

    /// Exit status only — for existence probes whose output is irrelevant.
    pub fn try_run_quiet(&self, args: &[&str]) -> Result<bool> {
        let o: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
        Ok(self.try_run(&o, &[], None, true, None)?.ok)
    }

    pub fn run(&self, args: &[&str]) -> Result<Vec<u8>> {
        let o: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
        self.run_os(&o, &[], None, true, false, None)
    }

    pub fn run_ok(&self, args: &[&str]) -> Result<bool> {
        self.try_run_quiet(args)
    }

    pub fn text(&self, args: &[&str], timeout: Option<Duration>) -> Result<String> {
        Ok(String::from_utf8_lossy(&self.run_os(
            &args.iter().map(OsStr::new).collect::<Vec<_>>(),
            &[],
            timeout,
            true,
            false,
            None,
        )?)
        .trim()
        .to_string())
    }

    /// Streaming variant for `git log`: stdout is piped and read incrementally
    /// so a 100k-commit repo never has to be materialised.
    pub fn spawn_stream_cfg(&self, args: &[&OsStr], cfg: &[String]) -> Result<Child> {
        let argv = self.argv(args, cfg);
        let mut cmd = base_cmd();
        // argv[0] is "git" for display; Command already names the program.
        cmd.args(&argv[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd.spawn().map_err(|_| spawn_error())
    }
}

/// `git -C <path> rev-parse --absolute-git-dir`, used for local-path input.
/// A local repo is read **in place** — the bare clone is never copied.
pub fn resolve_local_git_dir(path: &str) -> Result<PathBuf> {
    let mut cmd = base_cmd();
    for c in GIT_SAFE_CFG {
        cmd.arg("-c").arg(*c);
    }
    cmd.args(["-C", path, "rev-parse", "--absolute-git-dir"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let out = cmd.output().map_err(|_| spawn_error())?;
    if !out.status.success() {
        return Err(AuditError::with_hint(
            format!("Not a git repository: {}", safe(path)),
            safe(
                &String::from_utf8_lossy(&out.stderr)
                    .trim()
                    .chars()
                    .rev()
                    .take(200)
                    .collect::<String>()
                    .chars()
                    .rev()
                    .collect::<String>(),
            ),
        ));
    }
    Ok(PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv_of(allow_file: bool) -> Vec<String> {
        let g = Git {
            git_dir: Some(PathBuf::from("/tmp/x.git")),
            allow_file,
            mailmap_file: None,
        };
        g.argv(&[OsStr::new("rev-parse"), OsStr::new("HEAD")], &[])
            .iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn every_safe_switch_is_present_in_every_invocation() {
        for allow_file in [false, true] {
            let a = argv_of(allow_file);
            for c in GIT_SAFE_CFG {
                assert!(a.contains(&c.to_string()), "missing {c} in {a:?}");
            }
            assert_eq!(a[0], "git");
            assert_eq!(a[1], "--git-dir");
            assert_eq!(a[3], "-c");
            assert_eq!(a[4], "core.hooksPath=/dev/null");
            assert!(a.contains(&"protocol.allow=never".to_string()));
            assert!(a.contains(&"protocol.file.allow=always".to_string()) == allow_file);
            assert_eq!(a.last().unwrap(), "HEAD");
        }
    }

    #[test]
    fn env_strips_git_dir_and_pins_locale() {
        let base = [
            ("GIT_DIR", "/somewhere/else"),
            ("GIT_INDEX_FILE", "/tmp/idx"),
            ("GIT_WORK_TREE", "/tmp/wt"),
            ("GIT_ALTERNATE_OBJECT_DIRECTORIES", "/tmp/o"),
            ("GIT_OBJECT_DIRECTORY", "/tmp/oo"),
            ("PATH", "/usr/bin"),
        ]
        .map(|(k, v)| (std::ffi::OsString::from(k), std::ffi::OsString::from(v)));
        let e: Vec<(String, String)> = git_env_from(base)
            .iter()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.to_string_lossy().into_owned(),
                )
            })
            .collect();
        for gone in [
            "GIT_DIR",
            "GIT_INDEX_FILE",
            "GIT_WORK_TREE",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_OBJECT_DIRECTORY",
        ] {
            assert!(!e.iter().any(|(k, _)| k == gone), "{gone} leaked: {e:?}");
        }
        assert!(
            e.contains(&("PATH".into(), "/usr/bin".into())),
            "unrelated vars must survive"
        );
        assert!(e.contains(&("LC_ALL".into(), "C".into())));
        assert!(e.contains(&("GIT_TERMINAL_PROMPT".into(), "0".into())));
        assert!(e.contains(&("GIT_OPTIONAL_LOCKS".into(), "0".into())));
        assert!(e.contains(&("GIT_PAGER".into(), "cat".into())));
        assert!(e.contains(&("GIT_SSH_COMMAND".into(), "ssh -o BatchMode=yes".into())));
    }

    #[test]
    fn an_existing_ssh_command_is_not_overridden() {
        let base = [("GIT_SSH_COMMAND", "ssh -i /my/key")]
            .map(|(k, v)| (std::ffi::OsString::from(k), std::ffi::OsString::from(v)));
        let e = git_env_from(base);
        let v = e
            .iter()
            .find(|(k, _)| k == "GIT_SSH_COMMAND")
            .unwrap()
            .1
            .to_string_lossy()
            .into_owned();
        assert_eq!(v, "ssh -i /my/key");
    }

    #[test]
    fn failures_are_explained_not_dumped() {
        assert!(
            explain_git_failure("ERROR: Repository not found.").contains("Repository not found")
        );
        assert!(
            explain_git_failure("fatal: Authentication failed for 'https://x'")
                .contains("Authentication failed")
        );
        assert!(
            explain_git_failure("git@github.com: Permission denied (publickey).")
                .contains("SSH public key")
        );
        assert!(
            explain_git_failure("fatal: unable to access ... Could not resolve host")
                .contains("Network unavailable")
        );
        assert_eq!(explain_git_failure("fatal: something else entirely"), "");
    }

    #[test]
    fn a_real_git_call_round_trips() {
        let g = Git::new(None);
        let v = g.text(&["--version"], None).unwrap();
        assert!(v.starts_with("git version"), "{v}");
        // rev-parse echoes unresolvable-but-parseable args and exits 0, so
        // this is the check that actually exercises the non-zero path.
        assert!(!g
            .run_ok(&["rev-parse", "--verify", "refs/heads/definitely-not-here"])
            .unwrap());
    }
}
