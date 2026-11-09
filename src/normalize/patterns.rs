//! Classification patterns. Every one is compiled once, lazily, and shared.
//!
//! These strings are also passed *into* SQLite as bind parameters (see
//! `store::udf`), so they must stay byte-identical to what Python compiled.

use regex::{Regex, RegexBuilder};
use std::sync::OnceLock;

/// Version lockfiles, build output, binary/asset files, and Eclipse/Tycho build
/// descriptors. Excluded files are disclosed in the report, never silently
/// dropped — their line count dwarfs the real signal otherwise.
pub const SKIP_FILE_SRC: &str = concat!(
    r"(^|/)(package-lock\.json|yarn\.lock|pnpm-lock\.yaml|poetry\.lock|Cargo\.lock|go\.sum|",
    r"Gemfile\.lock|composer\.lock|libs\.versions\.toml)$",
    r"|\.min\.(js|css)$|(^|/)(vendor|node_modules|dist|target|build)/",
    r"|\.(ai|psd|png|jpe?g|gif|ico|svg|pdf|woff2?|ttf)$",
    r"|(^|/)[^/]+\.(target|tpd|product|classpath|project)$",
);

pub const DEFAULT_BOT_RE: &str = concat!(
    r"\[bot\]|dependabot|renovate|greenkeeper|github[- ]actions|snyk[- ]bot|semantic[- ]release|",
    r"pre-commit-ci|allcontributors|imgbot|mergify|(^|[\s_-])bot$|^bot($|[\s_-])|-robot$|^web-flow$",
);

/// Two capture groups: optional numeric UID, then login. GitHub noreply
/// addresses look like `114227601+hiddify-com@users.noreply.github.com` —
/// **the UID does not change on rename, the login does**.
pub const GH_NOREPLY_RE: &str =
    r"^(?:(\d+)\+)?([a-z0-9](?:[a-z0-9-]*[a-z0-9])?(?:\[bot\])?)@users\.noreply\.github\.com$";

/// Tightened: `git@`-style "account is the domain" is only anonymous when the
/// domain has no dot. The loose rule flagged `git@philippkeck.de` — a real
/// contributor — as a bot, and bus factor's worst failure is dropping a person.
pub const GENERIC_EMAIL: &str = concat!(
    r"^$|\(none\)|",
    r"^(noreply|no-reply|unknown|none|nobody|root|admin|user|git|me)@[^.]*$|",
    r"^(noreply|no-reply|unknown|none|nobody|root|admin|user|git|me)@",
    r"(localhost|localhost\.localdomain|example\.(com|org|net))$|",
    r"@(localhost|localhost\.localdomain|example\.(com|org|net))$|",
    r"\.(local|localdomain|lan|internal|invalid)$",
);

/// The only two patterns ever handed to the `rx` UDF.
pub const FIX_RE: &str = r"\b(fix(e[sd])?|bug(fix)?|hotfix|patch|regression)\b";
pub const FIRE_RE: &str = r"^\s*(revert|reverts)\b|\b(hotfix|rollback|back(ed)? out)\b";

macro_rules! lazy_ci {
    ($name:ident, $src:expr) => {
        pub fn $name() -> &'static Regex {
            static RE: OnceLock<Regex> = OnceLock::new();
            RE.get_or_init(|| {
                RegexBuilder::new($src)
                    .case_insensitive(true)
                    .build()
                    .expect($src)
            })
        }
    };
}

lazy_ci!(skip_file_re, SKIP_FILE_SRC);
lazy_ci!(gh_noreply_re, GH_NOREPLY_RE);
lazy_ci!(generic_email_re, GENERIC_EMAIL);

/// The `default` bot pattern. Extra `--bot-pattern` values are alternated onto
/// the front of this, exactly as Python did: `"|".join([DEFAULT_BOT_RE] + extra)`.
pub fn default_bot_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        RegexBuilder::new(DEFAULT_BOT_RE)
            .case_insensitive(true)
            .build()
            .unwrap()
    })
}

/// Compile `--bot-pattern` extras onto `DEFAULT_BOT_RE`.
pub fn compile_bot_re(extra: &[String]) -> crate::error::Result<Regex> {
    let mut parts = vec![DEFAULT_BOT_RE.to_string()];
    parts.extend_from_slice(extra);
    RegexBuilder::new(&parts.join("|"))
        .case_insensitive(true)
        .build()
        .map_err(|e| {
            crate::error::AuditError::new(format!("--bot-pattern is not a valid regex: {e}"))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skip_file_matches_lockfiles_and_generated() {
        let re = skip_file_re();
        for hit in [
            "package-lock.json",
            "a/b/pnpm-lock.yaml",
            "sub/go.sum",
            "web/x.min.js",
            "vendor/lib/x.py",
            "node_modules/a/b.js",
            "dist/main.js",
            "build/out.js",
            "app/icon.png",
            "proj/x.project",
            "proj/thing.classpath",
            "proj/a/b.target",
        ] {
            assert!(re.is_match(hit), "expected SKIP_FILE to match {hit}");
        }
        // A dotfile like `.project` has no stem, so `[^/]+\.` cannot match it —
        // Python rejects it too; the rule is not "any dotfile", it is "any file
        // whose name ends in one of these extensions".
        for miss in [
            "src/main.py",
            "Cargo.toml",
            "README.md",
            "src/target.go",
            "main.js",
            "proj/.project",
            ".classpath",
        ] {
            assert!(!re.is_match(miss), "expected SKIP_FILE NOT to match {miss}");
        }
    }

    #[test]
    fn noreply_splits_uid_from_login() {
        let re = gh_noreply_re();
        let c = re
            .captures("114227601+hiddify-com@users.noreply.github.com")
            .unwrap();
        assert_eq!(c.get(1).unwrap().as_str(), "114227601");
        assert_eq!(c.get(2).unwrap().as_str(), "hiddify-com");

        // Legacy form has no UID: group 1 is None, group 2 falls back to login.
        let c = re.captures("carol@users.noreply.github.com").unwrap();
        assert!(c.get(1).is_none());
        assert_eq!(c.get(2).unwrap().as_str(), "carol");

        // Bot accounts carry a [bot] suffix.
        let c = re
            .captures("49699333+dependabot[bot]@users.noreply.github.com")
            .unwrap();
        assert_eq!(c.get(1).unwrap().as_str(), "49699333");
        assert_eq!(c.get(2).unwrap().as_str(), "dependabot[bot]");
    }

    #[test]
    fn generic_email_requires_dotless_domain() {
        let re = generic_email_re();
        for anon in [
            "",
            "root@localhost",
            "git@git",
            "a@example.com",
            "x@foo.local",
        ] {
            assert!(re.is_match(anon), "expected generic: {anon:?}");
        }
        // The regression this rule exists for: a real person's local git config.
        assert!(!re.is_match("git@philippkeck.de"));
        assert!(!re.is_match("someone@gmail.com"));
    }

    #[test]
    fn bot_pattern_catches_known_bots() {
        let re = default_bot_re();
        for bot in [
            "dependabot[bot]",
            "github-actions",
            "web-flow",
            "renovate-bot",
            "imgbot",
        ] {
            assert!(re.is_match(bot), "expected bot: {bot}");
        }
        for human in ["alice", "botanist", "Tunglies", "root"] {
            assert!(!re.is_match(human), "expected NOT bot: {human}");
        }
    }

    #[test]
    fn fix_re_uses_word_boundaries() {
        let re = RegexBuilder::new(FIX_RE)
            .case_insensitive(true)
            .build()
            .unwrap();
        assert!(re.is_match("fix the parser"));
        assert!(re.is_match("FIX-123"));
        assert!(re.is_match("bugfix"));
        // The reason this is a UDF rather than SQL LIKE: `LIKE 'fix%'` would
        // also count `prefix`.
        assert!(!re.is_match("prefix change"));
    }

    #[test]
    fn extra_bot_patterns_alternate_onto_the_default() {
        let re = compile_bot_re(&["^custom-ci$".to_string()]).unwrap();
        assert!(re.is_match("dependabot[bot]"));
        assert!(re.is_match("custom-ci"));
        assert!(!re.is_match("alice"));
        assert!(compile_bot_re(&["(".to_string()]).is_err());
    }
}
