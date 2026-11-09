//! Command line.
//!
//! `audit` is the default subcommand, so `repo-audit <repo>` and
//! `repo-audit --json <repo>` both work. Python did that by prepending
//! `"audit"` in `_normalize_argv` before `argparse` ran; clap cannot express a
//! defaulted subcommand, so the same pre-pass happens here — before `Cli`
//! parsing, not inside it.

use crate::error::{AuditError, Result};
use crate::index::{index_repo, IndexOptions};
use crate::report::{identities, json, terminal};
use crate::util::text::{chars_take, safe};
use crate::VERSION;
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;
use std::time::Duration;

const SUBCOMMANDS: [&str; 4] = ["audit", "index", "identities", "selftest"];

#[derive(Parser, Debug)]
#[command(
    name = "repo-audit",
    version = VERSION,
    about = "Git repository health check · five audit analyses",
    disable_help_subcommand = true
)]
pub struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Fetch, index, and print the five-part audit report (default command)
    Audit {
        #[command(flatten)]
        common: Common,
    },
    /// Fetch and index only; do not print a report
    Index {
        #[command(flatten)]
        common: Common,
    },
    /// Show merged author identities
    Identities {
        #[command(flatten)]
        common: Common,
        /// Include authors with no merged aliases
        #[arg(long = "all")]
        all: bool,
    },
    /// Run built-in tests
    Selftest,
}

#[derive(Args, Debug)]
struct Common {
    /// URL / git@host:owner/repo / owner/repo / local path
    repo: String,
    /// Branch or commit to analyze (default: HEAD)
    #[arg(long, default_value = "HEAD")]
    r#ref: String,
    /// Discard the cached index and rebuild it
    #[arg(long)]
    reindex: bool,
    /// Do not merge authors by name (more conservative)
    #[arg(long = "no-name-merge")]
    no_name_merge: bool,
    /// Additional bot-detection regex; may be repeated
    #[arg(long = "bot-pattern", value_name = "REGEX")]
    bot_pattern: Vec<String>,
    /// Additional mailmap override file (default: ~/.config/repo-audit/mailmap)
    #[arg(long, value_name = "FILE")]
    mailmap: Option<String>,
    /// Cache directory (macOS default: ~/Library/Caches/repo-audit)
    #[arg(long = "cache-dir", value_name = "DIR")]
    cache_dir: Option<PathBuf>,
    /// Staging directory for clone --bare: clone here, then atomically rename into the cache.
    /// Required only for remote repositories; unwritable or cross-volume staging fails without fallback.
    #[arg(long, value_name = "DIR")]
    tmp_dir: Option<PathBuf>,
    /// Clone/fetch timeout in seconds
    #[arg(long, default_value_t = 1800.0)]
    timeout: f64,
    #[arg(short, long)]
    quiet: bool,
    /// Output JSON
    #[arg(long)]
    json: bool,
}

/// Insert `audit` when the first argument is not a subcommand, so the shorthand
/// forms keep working.
pub fn normalize_argv<I, S>(argv: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let a: Vec<String> = argv.into_iter().map(Into::into).collect();
    match a.first() {
        Some(f) if SUBCOMMANDS.contains(&f.as_str()) => a,
        _ => {
            let mut v = vec!["audit".to_string()];
            v.extend(a);
            v
        }
    }
}

fn opts_of(c: &Common) -> Result<IndexOptions> {
    // `Duration::from_secs_f64` panics on a negative/NaN/overflowing value, and
    // the panic would be mislabelled "a defect" by main's catch_unwind. This is
    // a usage error, so reject it here where an AuditError is still available.
    let timeout = Duration::try_from_secs_f64(c.timeout)
        .ok()
        .filter(|d| !d.is_zero())
        .ok_or_else(|| {
            AuditError::with_hint(
                format!(
                    "--timeout must be a positive number of seconds, got {}",
                    c.timeout
                ),
                "Example: --timeout 3600",
            )
        })?;
    Ok(IndexOptions {
        git_ref: c.r#ref.clone(),
        reindex: c.reindex,
        name_merge: !c.no_name_merge,
        bot_patterns: c.bot_pattern.clone(),
        mailmap_file: c.mailmap.clone(),
        cache_dir: c.cache_dir.clone(),
        tmp_dir: c.tmp_dir.clone(),
        timeout,
        quiet: c.quiet,
        ..Default::default()
    })
}

/// clap's `try_parse_from` consumes argv[0] as the program name, so the
/// normalised argv (which starts with the subcommand) has to be prefixed.
pub fn parse(argv: Vec<String>) -> std::result::Result<Cli, clap::Error> {
    let mut full = vec![env!("CARGO_PKG_NAME").to_string()];
    full.extend(argv);
    Cli::try_parse_from(full)
}

pub fn run(argv: Vec<String>) -> i32 {
    let cli = match parse(argv) {
        Ok(c) => c,
        Err(e) => {
            // clap's own exit already formats help/version; only genuine
            // failures reach here.
            let _ = e.print();
            return if e.use_stderr() { 2 } else { 0 };
        }
    };
    match cli.cmd {
        Cmd::Selftest => crate::selftest::run(),
        Cmd::Audit { common } => audit(&common),
        Cmd::Index { common } => index_only(&common),
        Cmd::Identities { common, all } => identities_cmd(&common, all),
    }
}

fn audit(c: &Common) -> i32 {
    let opts = match opts_of(c) {
        Ok(o) => o,
        Err(e) => return fail(&e),
    };
    match index_repo(&c.repo, &opts) {
        Ok(res) => {
            if c.json {
                println!("{}", json::to_pretty(&json::render_json(&res)));
            } else {
                terminal::render_report(&res);
            }
            0
        }
        Err(e) => fail(&e),
    }
}

fn index_only(c: &Common) -> i32 {
    let opts = match opts_of(c) {
        Ok(o) => o,
        Err(e) => return fail(&e),
    };
    match index_repo(&c.repo, &opts) {
        Ok(res) => {
            println!(
                "OK  {}  {}  ({}, {:.1}s)",
                safe(&res.display),
                chars_take(&res.head, 10),
                // The raw action name, not `label()`: Python's `index` branch
                // prints `res.action` directly, so it stays `noop`/`full` here
                // while the progress line on stderr shows `no new commits`/`full`.
                res.action.as_str(),
                res.seconds
            );
            0
        }
        Err(e) => fail(&e),
    }
}

fn identities_cmd(c: &Common, all: bool) -> i32 {
    let opts = match opts_of(c) {
        Ok(o) => o,
        Err(e) => return fail(&e),
    };
    match index_repo(&c.repo, &opts) {
        Ok(res) => match identities::render_identities(&res, all) {
            Ok(out) => {
                println!("{out}");
                0
            }
            Err(e) => fail(&e),
        },
        Err(e) => fail(&e),
    }
}

/// `AuditError(message, hint)` prints message + hint, never a traceback.
fn fail(e: &AuditError) -> i32 {
    eprintln!("Error: {}", e.message);
    if !e.hint.is_empty() {
        eprintln!("Hint: {}", e.hint);
    }
    2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_repo_argument_defaults_to_audit() {
        assert_eq!(
            normalize_argv(["../proxyServer/s-ui"]),
            vec!["audit", "../proxyServer/s-ui"]
        );
        // A leading flag is not a subcommand either.
        assert_eq!(
            normalize_argv(["--json", "x"]),
            vec!["audit", "--json", "x"]
        );
        assert_eq!(
            normalize_argv(Vec::<String>::new()),
            vec!["audit".to_string()]
        );
    }

    #[test]
    fn an_explicit_subcommand_is_left_alone() {
        for c in SUBCOMMANDS {
            assert_eq!(normalize_argv([c, "repo"]), vec![c, "repo"]);
        }
        // And so is something that merely looks like one in the wrong position.
        assert_eq!(normalize_argv(["index", "audit"]), vec!["index", "audit"]);
    }

    #[test]
    fn the_shared_options_parse_for_every_subcommand() {
        for cmd in ["audit", "index"] {
            let v = normalize_argv([
                cmd,
                "./r",
                "--ref",
                "v1",
                "--no-name-merge",
                "--bot-pattern",
                "^x$",
            ]);
            let cli = parse(v).unwrap();
            let c = match cli.cmd {
                Cmd::Audit { common } => common,
                Cmd::Index { common } => common,
                _ => unreachable!(),
            };
            assert_eq!(c.r#ref, "v1");
            assert!(c.no_name_merge);
            assert_eq!(c.bot_pattern, vec!["^x$".to_string()]);
            let o = opts_of(&c).unwrap();
            assert!(!o.name_merge);
            assert_eq!(o.timeout, Duration::from_secs(1800));
        }
    }

    #[test]
    fn identities_has_its_own_all_flag_and_audit_has_json() {
        let cli = parse(normalize_argv(["identities", "./r", "--all"])).unwrap();
        match cli.cmd {
            Cmd::Identities { all, common } => {
                assert!(all);
                assert!(!common.json);
            }
            _ => panic!("expected identities"),
        }
        let cli = parse(normalize_argv(["--json", "./r"])).unwrap();
        match cli.cmd {
            Cmd::Audit { common } => assert!(common.json),
            _ => panic!("expected audit"),
        }
    }

    #[test]
    fn an_unknown_flag_is_an_error_not_a_silent_default() {
        assert!(parse(normalize_argv(["./r", "--nope"])).is_err());
        // A missing repo is an error too, not an empty spec.
        assert!(parse(vec!["audit".to_string()]).is_err());
    }

    /// Negative, zero and non-finite timeouts were a `Duration` panic caught by
    /// main's hook as "Internal error (this is a defect)"; a typo must stay a usage error.
    #[test]
    fn a_bad_timeout_is_a_usage_error_not_a_panic() {
        for v in ["-1", "0", "0.0", "nan", "inf", "-inf", "1e300"] {
            let cli = parse(normalize_argv(["audit", "./r", &format!("--timeout={v}")])).unwrap();
            let common = match cli.cmd {
                Cmd::Audit { common } => common,
                _ => unreachable!(),
            };
            let err = match opts_of(&common) {
                Err(e) => e,
                Ok(_) => panic!("expected --timeout={v} to be rejected"),
            };
            assert!(err.message.contains("--timeout"), "{v}: {}", err.message);
        }
        // The ordinary default still parses to 1800 s.
        let cli = parse(normalize_argv(["audit", "./r"])).unwrap();
        let common = match cli.cmd {
            Cmd::Audit { common } => common,
            _ => unreachable!(),
        };
        assert_eq!(opts_of(&common).unwrap().timeout, Duration::from_secs(1800));
    }

    #[test]
    fn selftest_takes_no_arguments() {
        assert!(parse(vec!["selftest".to_string()]).is_ok());
    }
}
