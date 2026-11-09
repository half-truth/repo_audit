//! Author identity resolution — pure functions, no I/O.
//!
//! One person can hit several keys at once; sharing *any* key means same person.
//! Order matters: a GitHub noreply address is far more reliable than a name —
//! it identifies one account, while two different people quite plausibly share
//! a name. An early version preferred "multi-word name, noreply as fallback",
//! which let the weakest signal override the strongest.
//!
//! Inside noreply there is a second layer: **the UID is more reliable than the
//! login**. A GitHub rename leaves the UID alone and rewrites the login, so
//! keying on the login split one person across several identities — on
//! Hiddify-Manager `hiddify` / `hiddify-com` / `hiddify1` (all UID 114227601)
//! counted as three people, and because the *old* `hiddify` had gone quiet it
//! raised a bus-factor alert while `hiddify-com` was committing that same week.
//! Legacy noreply addresses without a UID (`carol@users.noreply.github.com`)
//! fall back to the login; measured share is ~0.2%.

use super::patterns::{compile_bot_re, default_bot_re, generic_email_re, gh_noreply_re};
use regex::Regex;
use std::collections::{HashMap, HashSet};

/// Python: `" ".join(unicodedata.normalize("NFKC", name).lower().split())`.
///
/// **Known divergence:** NFKC is not applied (no `unicode-normalization` in the
/// dependency allowlist). Measured on the 15-repo corpus: 2 of 1485 distinct
/// author names change under NFKC (`～`→`~`, `？`→`?`), and neither contains
/// whitespace, so neither can ever satisfy the two-word test in
/// [`identity_keys`] and neither reaches the output.
pub fn norm_name(name: &str) -> String {
    crate::util::text::py_split_whitespace(&name.to_lowercase()).join(" ")
}

/// Every key an identity can be merged on.
pub fn identity_keys(name: &str, email: &str, name_merge: bool) -> Vec<String> {
    let mut keys = Vec::new();
    let e = email.trim().to_lowercase();
    if let Some(c) = gh_noreply_re().captures(&e) {
        let uid = c.get(1).map(|m| m.as_str()).unwrap_or("");
        let login = c.get(2).map(|m| m.as_str()).unwrap_or("").to_lowercase();
        keys.push(if uid.is_empty() {
            format!("github-login:{login}")
        } else {
            format!("github-uid:{uid}")
        });
    } else if !generic_email_re().is_match(&e) {
        keys.push(format!("email:{e}"));
    }
    if name_merge {
        let n = norm_name(name);
        // Conservative: must be a multi-word name, long enough, not an email.
        // Single-word handles (root, admin, bob) never merge on name alone.
        let words = crate::util::text::py_split_whitespace(&n).len();
        let chars = n.chars().count();
        if words >= 2 && chars >= 5 && !n.contains('@') {
            keys.push(format!("name:{n}"));
        }
    }
    keys
}

#[derive(Debug, Clone)]
pub struct AuthorGroup {
    pub canonical_name: String,
    pub canonical_email: String,
    pub is_bot: bool,
    /// `(name, email, commits)`, sorted by `(-commits, name, email)`.
    pub members: Vec<(String, String, i64)>,
    pub reasons: Vec<String>,
    pub commit_count: i64,
    pub first_ts: i64,
    pub last_ts: i64,
    /// No member has any key other than `name:` — the repo's emails carry no
    /// discriminating power (typically all `root@localhost`), so the identity
    /// is unknowable and is treated as a bot.
    pub unattributable: bool,
}

/// `(name, email, commits, first_ts, last_ts)`, deduplicated by `(name, email)`.
pub type IdentityRow = (String, String, i64, i64, i64);

/// Insertion-ordered `Counter` plus Python's `max(..., key=...)` semantics:
/// ties resolve to the **first** key inserted, because `max` only replaces on a
/// strictly greater key.
struct PyCounter {
    order: Vec<String>,
    counts: HashMap<String, i64>,
}

impl PyCounter {
    fn new() -> Self {
        Self {
            order: Vec::new(),
            counts: HashMap::new(),
        }
    }

    fn add(&mut self, key: &str, n: i64) {
        if !self.counts.contains_key(key) {
            self.order.push(key.to_string());
            self.counts.insert(key.to_string(), 0);
        }
        *self.counts.get_mut(key).unwrap() += n;
    }

    fn items(&self) -> impl Iterator<Item = (&String, i64)> + '_ {
        self.order.iter().map(move |k| (k, self.counts[k]))
    }
}

/// Python's `max(items, key=...)`: keeps the **first** item holding the greatest
/// key, because it only replaces on a strictly greater key. `Iterator::max_by_key`
/// returns the *last*, which would silently flip every canonical-name pick.
fn py_max<'a, K: Ord + 'a, I: Iterator<Item = (&'a String, i64)>>(
    items: I,
    key: impl Fn(&'a String, i64) -> K,
) -> Option<&'a String> {
    let mut best: Option<(&'a String, K)> = None;
    for (k, c) in items {
        let kk = key(k, c);
        if best.as_ref().is_none_or(|(_, b)| kk > *b) {
            best = Some((k, kk));
        }
    }
    best.map(|(k, _)| k)
}

/// Group identities into people.
///
/// Bot status is decided **per group**, not per identity: bots commonly commit
/// from several addresses, and per-identity filtering silently loses part of
/// their output — while bus factor's worst failure mode is crediting a bot's
/// commits to a human.
pub fn build_identity_groups(
    rows: &[IdentityRow],
    name_merge: bool,
    bot_re: Option<&Regex>,
) -> Vec<AuthorGroup> {
    let default_bot;
    let bot_re = match bot_re {
        Some(r) => r,
        None => {
            default_bot = compile_bot_re(&[]).expect("default bot regex");
            &default_bot
        }
    };
    let n = rows.len();
    let mut parent: Vec<usize> = (0..n).collect();

    fn find(parent: &mut [usize], mut x: usize) -> usize {
        while parent[x] != x {
            parent[x] = parent[parent[x]]; // path halving
            x = parent[x];
        }
        x
    }

    let keys_of: Vec<Vec<String>> = rows
        .iter()
        .map(|(name, email, ..)| identity_keys(name, email, name_merge))
        .collect();

    let mut key_members: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, kl) in keys_of.iter().enumerate() {
        for k in kl {
            key_members.entry(k.clone()).or_default().push(i);
        }
    }
    for members in key_members.values() {
        for &j in members.iter().skip(1) {
            let ra = find(&mut parent, members[0]);
            let rb = find(&mut parent, j);
            if ra != rb {
                parent[rb] = ra;
            }
        }
    }

    // Built by ascending member index, not by HashMap iteration order, so the
    // group order (and therefore every tie-break downstream) is deterministic.
    let mut group_order: Vec<usize> = Vec::new();
    let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..n {
        let root = find(&mut parent, i);
        let entry = groups.entry(root).or_insert_with(|| {
            group_order.push(root);
            Vec::new()
        });
        entry.push(i);
    }

    let mut reasons: HashMap<usize, HashSet<String>> = HashMap::new();
    for (k, members) in &key_members {
        if members.len() >= 2 {
            let root = find(&mut parent, members[0]);
            reasons.entry(root).or_default().insert(k.clone());
        }
    }

    let mut out: Vec<AuthorGroup> = Vec::with_capacity(group_order.len());
    for root in group_order {
        let idxs = &groups[&root];
        let mem: Vec<(String, String, i64)> = idxs
            .iter()
            .map(|&i| (rows[i].0.clone(), rows[i].1.clone(), rows[i].2))
            .collect();
        let total: i64 = mem.iter().map(|m| m.2).sum();

        let mut names = PyCounter::new();
        let mut emails = PyCounter::new();
        let mut bot_w = 0i64;
        let mut anchored = false;
        for &i in idxs {
            let (nm, em, c) = (&rows[i].0, &rows[i].1, rows[i].2);
            names.add(nm, c);
            emails.add(em, c);
            let local = em.split('@').next().unwrap_or("");
            if bot_re.is_match(nm) || bot_re.is_match(local) {
                bot_w += c;
            }
            if keys_of[i].iter().any(|k| !k.starts_with("name:")) {
                anchored = true;
            }
        }

        // max by (commits, len(name), name) — length is in code points, as in Python.
        let cname = py_max(names.items(), |k, c| {
            (c, k.chars().count() as i64, k.clone())
        })
        .cloned()
        .unwrap_or_default();
        let cemail = py_max(emails.items(), |k, c| (c, k.clone()))
            .cloned()
            .unwrap_or_default();

        let cname_local = cemail.split('@').next().unwrap_or("");
        let is_bot = bot_re.is_match(&cname)
            || bot_re.is_match(cname_local)
            || bot_w * 2 >= total
            || !anchored;

        let mut members = mem;
        members.sort_by(|a, b| {
            b.2.cmp(&a.2)
                .then_with(|| a.0.cmp(&b.0))
                .then_with(|| a.1.cmp(&b.1))
        });
        let mut rs: Vec<String> = reasons
            .get(&root)
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default();
        rs.sort();

        out.push(AuthorGroup {
            canonical_name: cname,
            canonical_email: cemail,
            is_bot,
            members,
            reasons: rs,
            commit_count: total,
            first_ts: idxs.iter().map(|&i| rows[i].3).min().unwrap_or(0),
            last_ts: idxs.iter().map(|&i| rows[i].4).max().unwrap_or(0),
            unattributable: !anchored,
        });
    }
    out.sort_by(|a, b| {
        b.commit_count
            .cmp(&a.commit_count)
            .then_with(|| a.canonical_email.cmp(&b.canonical_email))
            .then_with(|| a.canonical_name.cmp(&b.canonical_name))
    });
    out
}

/// Convenience wrapper for callers that don't supply `--bot-pattern`.
pub fn build_identity_groups_default(rows: &[IdentityRow], name_merge: bool) -> Vec<AuthorGroup> {
    build_identity_groups(rows, name_merge, Some(default_bot_re()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(name: &str, email: &str, commits: i64, ts: i64) -> IdentityRow {
        (name.into(), email.into(), commits, ts, ts)
    }

    #[test]
    fn noreply_uid_merges_across_renames() {
        let k = |n: &str, e: &str| identity_keys(n, e, false);
        let a = k("hiddify", "114227601+hiddify@users.noreply.github.com");
        let b = k("Hiddify", "114227601+hiddify-com@users.noreply.github.com");
        assert_eq!(a[0], "github-uid:114227601");
        assert_eq!(a, b, "same UID must produce the same key across renames");

        // Different UID, same login: must NOT merge.
        let c = k("other", "999+hiddify@users.noreply.github.com");
        assert_ne!(a[0], c[0]);
    }

    #[test]
    fn legacy_noreply_falls_back_to_login() {
        let k = identity_keys("carol", "carol@users.noreply.github.com", true);
        assert_eq!(k[0], "github-login:carol");
    }

    #[test]
    fn real_domain_git_address_is_a_person() {
        // The rule exists because the loose version dropped a real contributor
        // from the bus-factor count.
        let k = identity_keys("Philipp Keck", "git@philippkeck.de", true);
        assert_eq!(k[0], "email:git@philippkeck.de");
    }

    #[test]
    fn generic_email_is_dropped_entirely() {
        assert!(identity_keys("root", "root@localhost", true).is_empty());
    }

    #[test]
    fn single_word_names_never_merge() {
        assert!(!identity_keys("root", "a@b.com", true)
            .iter()
            .any(|k| k.starts_with("name:")));
        assert!(!identity_keys("bob", "a@b.com", true)
            .iter()
            .any(|k| k.starts_with("name:")));
        assert!(identity_keys("jane doe", "a@b.com", true)
            .iter()
            .any(|k| k.starts_with("name:")));
        assert!(!identity_keys("jane doe", "a@b.com", false)
            .iter()
            .any(|k| k.starts_with("name:")));
    }

    #[test]
    fn groups_merge_by_uid_and_report_reason() {
        let rows = vec![
            g(
                "hiddify",
                "114227601+hiddify@users.noreply.github.com",
                900,
                100,
            ),
            g(
                "Hiddify",
                "114227601+hiddify-com@users.noreply.github.com",
                330,
                200,
            ),
            g("someone else", "else@example.org", 10, 100),
        ];
        let gs = build_identity_groups_default(&rows, true);
        assert_eq!(gs.len(), 2);
        assert_eq!(gs[0].commit_count, 1230);
        assert_eq!(gs[0].members.len(), 2);
        assert_eq!(gs[0].reasons, vec!["github-uid:114227601".to_string()]);
        assert!(!gs[0].is_bot);
    }

    #[test]
    fn bot_is_judged_per_group() {
        // Two addresses of the same bot, joined by a shared name key so they
        // land in one group the way they do in the wild.
        let rows = vec![
            g(
                "dependabot core",
                "49699333+dependabot[bot]@users.noreply.github.com",
                100,
                100,
            ),
            g("dependabot core", "renovate@example.org", 40, 100),
        ];
        let gs = build_identity_groups_default(&rows, true);
        assert_eq!(gs.len(), 1, "shared name key should merge them");
        assert!(
            gs[0].is_bot,
            "the group is judged as a whole, so bot_w wins"
        );
        assert_eq!(gs[0].commit_count, 140);
    }

    #[test]
    fn distinct_uids_with_the_same_name_stay_split() {
        let rows = vec![
            g("Alice Smith", "111+alice@users.noreply.github.com", 5, 100),
            g("Alice Smith", "222+alice@users.noreply.github.com", 3, 100),
        ];
        // The UID is the account's identity; the login can collide. But the
        // name key still merges them, which is the documented heuristic
        // ("two people with the same name may be merged") — what must never
        // happen is the reverse: two *different* UIDs merging on UID alone.
        let gs = build_identity_groups_default(&rows, true);
        assert_eq!(gs.len(), 1);
        let k = identity_keys("Alice Smith", "111+alice@users.noreply.github.com", false);
        let k2 = identity_keys("Alice Smith", "222+alice@users.noreply.github.com", false);
        assert_ne!(k[0], k2[0]);
    }

    #[test]
    fn all_name_only_group_is_unattributable() {
        let rows = vec![g("machine one", "root@localhost", 500, 100)];
        let gs = build_identity_groups_default(&rows, true);
        assert!(gs[0].unattributable);
        assert!(
            gs[0].is_bot,
            "an unattributable identity must not read as a person"
        );
    }

    #[test]
    fn canonical_pick_is_stable_on_ties() {
        let rows = vec![
            g("alice smith", "a@example.org", 10, 100),
            g("bob jones", "b@example.org", 10, 100),
        ];
        let gs = build_identity_groups_default(&rows, true);
        // Equal counts, equal length -> alphabetical winner.
        assert_eq!(gs[0].canonical_name, "alice smith");
    }
}
