//! The five analyses.
//!
//! Every one is a read-only query against the SQLite cache: file-level rankings
//! need whole-history statistics over hundreds of thousands of rows, and holding
//! those in memory is not an option. SQL does the grouping and filtering; the
//! `rx` / `isrevert` UDFs do the regexes.
//!
//! **Tie-breaking is load-bearing.** Python's `Counter.most_common()` breaks
//! ties by first insertion, and insertion order here is the order SQLite returns
//! `GROUP BY` rows in. Ties are real: 12 of the 15 cached repositories have
//! files with identical churn, so adding an `ORDER BY` to "stabilise" a ranking
//! would silently change the report.

use crate::normalize::patterns::{default_bot_re, skip_file_re, FIRE_RE, FIX_RE};
use crate::report::layout::{self, sub_head};
use crate::store::sqlite::SqliteStore;
use crate::util::civil::{civil_from_days, days_from_unix};
use crate::util::clock::{fmt_ts, months_ago};
use crate::util::text::{bar, display_width, hbar, pad_display, safe, truncate_display};
use rusqlite::params;
use std::collections::{HashMap, HashSet};

pub const RISK_TOP: usize = 8;
pub const RISK_ALERT: f64 = 0.75;
pub const RISK_WARN: f64 = 0.40;
/// Below this many files a percentile ranking has no discriminating power.
pub const RISK_MIN_FILES: usize = 15;
/// A file touched fewer times than this has a fix *rate* built on too few
/// observations to rank: 2-of-3 and 20-of-30 both read as "mostly fixes".
pub const DENSITY_MIN_TOUCH: f64 = 10.0;

pub fn mk_verdict(level: &str) -> &'static str {
    match level {
        "ok" => "🟢 Healthy",
        "warn" => "🟡 Warning",
        "alert" => "🔴 Alert",
        _ => "⚪ Insufficient data",
    }
}

/// A Python `Counter`: insertion-ordered, `most_common` breaks ties by first
/// insertion. `f64` rather than an integer scale because Python accumulated
/// float sigmoid weights here and `%.2f` on the result is part of the output.
#[derive(Default)]
pub struct PyCounter {
    order: Vec<String>,
    counts: HashMap<String, f64>,
}

impl PyCounter {
    pub fn add(&mut self, key: &str, n: f64) {
        if !self.counts.contains_key(key) {
            self.order.push(key.to_string());
            self.counts.insert(key.to_string(), 0.0);
        }
        *self.counts.get_mut(key).unwrap() += n;
    }

    pub fn get(&self, key: &str) -> f64 {
        *self.counts.get(key).unwrap_or(&0.0)
    }

    /// `Counter[f]` — a missing key is a `KeyError` in Python, not a zero.
    pub fn index(&self, key: &str) -> f64 {
        *self
            .counts
            .get(key)
            .unwrap_or_else(|| panic!("KeyError: {key:?}"))
    }

    pub fn len(&self) -> usize {
        self.order.len()
    }

    /// Keys in insertion order, as `Counter.keys()` yields them.
    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.order.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    pub fn sum(&self) -> f64 {
        self.counts.values().sum()
    }

    /// Descending by count, ties by first insertion — Python's `most_common`.
    pub fn most_common(&self) -> Vec<(String, f64)> {
        let mut v: Vec<(String, f64)> = self
            .order
            .iter()
            .map(|k| (k.clone(), self.counts[k]))
            .collect();
        // A stable sort keeps ties in insertion order, which is exactly what
        // `Counter.most_common` does.
        v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        v
    }
}

fn exclude_skipped(skipped: &PyCounter) {
    if skipped.is_empty() {
        return;
    }
    let top = skipped
        .most_common()
        .into_iter()
        .take(3)
        .map(|(f, n)| format!("{} {} lines", safe(&f), n as i64))
        .collect::<Vec<_>>()
        .join(", ");
    let more = if skipped.len() > 3 { ", ..." } else { "" };
    println!(
        "  ⓘ Excluded {} lockfiles or build manifests ({} lines total): {}{}",
        skipped.len(),
        skipped.sum() as i64,
        top,
        more
    );
}

/// 1/5 churn hotspots. Only files that still exist at HEAD, non-binary, with
/// pre-rename history folded into the current path.
///
/// Returns `(churn, touch)`. `touch` — commits per file, SKIP_FILE paths
/// already excluded — is what 3/5 divides by to get a fix *rate*; it used to be
/// a local that died with the call, and the density ranking needs it.
pub fn audit_churn(store: &SqliteStore) -> (PyCounter, PyCounter) {
    let mut ch = PyCounter::default();
    let mut touch = PyCounter::default();
    let mut dropped = PyCounter::default();
    {
        // No ORDER BY: see the module note on tie-breaking.
        let mut s = store
            .conn
            .prepare(
                "SELECT f.path_current, SUM(f.added+f.deleted), COUNT(*) FROM file_changes f
                 JOIN commits c ON c.sha=f.sha JOIN files_at_head h ON h.path=f.path_current
                 WHERE c.is_merge=0 AND f.is_binary=0 GROUP BY f.path_current",
            )
            .unwrap();
        let rows = s
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })
            .unwrap();
        for row in rows {
            let (f, n, t) = row.unwrap();
            if skip_file_re().is_match(&f) {
                dropped.add(&f, n as f64);
                continue;
            }
            // Python assigns (`ch[f] = n`); GROUP BY makes the key unique, so
            // add-and-accumulate is observationally identical.
            ch.add(&f, n as f64);
            touch.add(&f, t as f64);
        }
    }
    layout::section(
        1,
        "Churn hotspots",
        "HEAD files only; pre-rename history folded into current paths",
    );
    let top: Vec<(String, f64)> = ch.most_common().into_iter().take(10).collect();
    let max_churn = top.first().map(|(_, n)| *n).unwrap_or(0.0);
    for (f, n) in &top {
        println!(
            "  {:>6} lines  {}  {:>4} commits  {}",
            *n as i64,
            bar(*n, max_churn, 20),
            touch.get(f) as i64,
            safe(f)
        );
    }
    exclude_skipped(&dropped);
    (ch, touch)
}

pub struct BusRow {
    pub name: String,
    pub n: i64,
    pub last: i64,
    pub aliases: i64,
}

/// 2/5 bus factor — how many people it takes to cover half the commits, after
/// bots are removed and identities merged.
pub fn audit_bus(store: &SqliteStore) {
    let rows = bus_rows(store);
    // Python: `sum(...) or 1` — an empty repo still divides.
    let total = (rows.iter().map(|r| r.n).sum::<i64>() as f64).max(1.0);
    layout::section(
        2,
        "Bus factor",
        "By commit count (bots excluded; same-name/same-email/noreply identities merged)",
    );
    let top_share_for_bar = rows.first().map(|r| r.n as f64).unwrap_or(1.0).max(1.0);
    let mut acc = 0f64;
    let mut truck = 0i64;
    for (i, r) in rows.iter().enumerate() {
        if acc < total * 0.5 {
            truck += 1;
        }
        acc += r.n as f64;
        if i < 5 {
            let share = r.n as f64 * 100.0 / total;
            println!(
                "  {:>5.1}%  {}  {:>5}  {}  last commit {}{}",
                share,
                bar(r.n as f64, top_share_for_bar, 10),
                r.n,
                pad_display(&truncate_display(&safe(&r.name), 24), 24),
                fmt_ts(Some(r.last)),
                if r.aliases > 1 {
                    format!("   {} aliases", r.aliases)
                } else {
                    String::new()
                }
            );
        }
    }
    let top_share = match rows.first() {
        Some(r) => r.n as f64 / total,
        None => 0.0,
    };
    let idle = rows.first().map(|r| months_ago(r.last)).unwrap_or(0);
    let level = if top_share > 0.8 || idle >= 6 {
        "alert"
    } else if top_share >= 0.5 {
        "warn"
    } else {
        "ok"
    };
    // "No commits for 0 months" reads as a contradiction; say the positive instead.
    let idle_txt = if idle == 0 {
        "committed within the last month".to_string()
    } else {
        format!("no commits for {idle} months")
    };
    println!(
        "  Minimum number of people to cover 50% of commits = {}; top author accounts for {:.0}%, {} → {}",
        truck,
        top_share * 100.0,
        idle_txt,
        mk_verdict(level)
    );
    if level != "ok" && idle >= 6 {
        println!("  ※ Top author has been inactive for a long time: knowledge may be lost, not merely concentrated");
    }
}

/// The author ranking behind 2/5 and the JSON output.
pub fn bus_rows(store: &SqliteStore) -> Vec<BusRow> {
    let mut out = Vec::new();
    {
        let mut s = store
            .conn
            .prepare(
                "SELECT a.canonical_name, COUNT(*) n, MAX(c.author_ts), json_array_length(a.aliases_json)
                 FROM commits c JOIN authors a USING(author_id)
                 WHERE c.is_merge=0 AND a.is_bot=0 GROUP BY a.author_id ORDER BY n DESC",
            )
            .unwrap();
        let rows = s
            .query_map([], |r| {
                Ok(BusRow {
                    name: r.get(0)?,
                    n: r.get(1)?,
                    last: r.get(2)?,
                    aliases: r.get(3)?,
                })
            })
            .unwrap();
        for r in rows {
            out.push(r.unwrap());
        }
    }
    // SQLite leaves `ORDER BY n DESC` ties in whatever order the grouping
    // produced, exactly as Python's did. Do **not** add a tie-break here: the
    // report is defined as a byte-for-byte replica, and a "helpful" secondary
    // sort would be a silent behaviour change.
    out
}

/// 3/5 bug clusters, weighted by recency (a Bugspots-style sigmoid centred on
/// the timeline's midpoint), then a fix-*rate* ranking.
pub fn audit_bugs(store: &SqliteStore, touch: &PyCounter) {
    let (ts0_raw, ts1_raw): (Option<i64>, Option<i64>) = store
        .conn
        .query_row(
            "SELECT MIN(author_ts), MAX(author_ts) FROM commits WHERE is_merge=0",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    let ts0 = ts0_raw.unwrap_or(0);
    let span = ((ts1_raw.unwrap_or(0) - ts0) as f64).max(1.0);
    let mut score = PyCounter::default();
    let mut cnt = PyCounter::default();
    {
        let mut s = store
            .conn
            .prepare(
                "SELECT f.path_current, c.author_ts FROM file_changes f JOIN commits c ON c.sha=f.sha
                 JOIN files_at_head h ON h.path=f.path_current WHERE c.is_merge=0 AND f.is_binary=0
                   AND rx(?1, c.subject || char(10) || c.body)",
            )
            .unwrap();
        let rows = s
            .query_map(params![FIX_RE], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })
            .unwrap();
        for row in rows {
            let (f, ts) = row.unwrap();
            // Python: `1 / (1 + math.exp(-12 * (ts - ts0) / span + 12))` — the
            // exponent runs from +12 at the start of history to -12 at the end,
            // so recent fixes weigh ~0.5 and ancient ones ~0. Inverting the
            // sign here would rank old files as the hot spots.
            let x = -12.0 * (ts - ts0) as f64 / span + 12.0;
            score.add(&f, 1.0 / (1.0 + x.exp()));
            cnt.add(&f, 1.0);
        }
    }
    let fixes: i64 = store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM commits WHERE is_merge=0 AND rx(?1, subject || char(10) || body)",
            params![FIX_RE],
            |r| r.get(0),
        )
        .unwrap();
    let n_com: i64 = store
        .conn
        .query_row("SELECT COUNT(*) FROM commits WHERE is_merge=0", [], |r| {
            r.get(0)
        })
        .unwrap();
    let n_com = if n_com == 0 { 1 } else { n_com };
    layout::section(
        3,
        "Bug clusters",
        &format!(
            "{} fix commits ({:.0}%); fix density = fixes / touches",
            fixes,
            fixes as f64 * 100.0 / n_com as f64
        ),
    );

    // REDESIGNED 2026-10-05. The old score was `churn percentile × bug
    // percentile`, and it measured the wrong thing: churn percentile is 0.59–0.84
    // correlated with "how many times the file was touched at all" across all 15
    // targets — *more* than with "how many times it was fixed". So the ranking
    // was largely a restatement of item 1, and lockfiles and dependabot-bumped
    // manifests took the top slots (`go.mod` led sing-box, 3x-ui and v2rayN).
    //
    // The fix rate — fixes over touches — is not collinear with churn, so it
    // surfaces the files that are actually re-broken. Measured: `go.mod`,
    // `go.sum` and `.github/workflows/build.yml` all leave the top slots.
    //
    // There is deliberately **no 🟢/🟡/🔴 here**. The obvious test — "are fixes
    // concentrated in more files than chance?" — has a false null hypothesis:
    // fix commits touch 1.3–4.8 candidate files each (flclash: 14), so per-file
    // independence is violated and any file-level test over-rejects. Measured
    // k/λ = 3.5–27 against a Poisson(m·α) null — i.e. every repository "fails"
    // for a reason that is an artefact of the model, not of the code. The
    // ranking carries the information; a tier on top of it would not.
    let cand: Vec<(f64, f64, String)> = score
        .keys()
        .filter_map(|f| {
            let t = touch.get(f);
            // `touch` already excludes SKIP_FILE paths, so membership here is
            // also the lockfile/generated-output filter.
            if t < DENSITY_MIN_TOUCH {
                return None;
            }
            Some((cnt.get(f) / t, cnt.get(f), f.clone()))
        })
        .collect();

    if cand.len() < RISK_MIN_FILES {
        sub_head(&format!(
            "Fix density: ⚪ insufficient data ({} files with ≥{} touches; too few to rank)",
            cand.len(),
            DENSITY_MIN_TOUCH as i64
        ));
        for (d, k, f) in cand.iter().take(3) {
            println!(
                "    ‣ {d:.2}  {} ({k:.0} fixes / {} touches)",
                safe(f),
                touch.get(f) as i64
            );
        }
        return;
    }

    // Density desc, then fix count desc, then path desc. A new ranking has no
    // Python tie-break to reproduce, so this is chosen for determinism only.
    let mut ranked = cand;
    ranked.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| b.2.cmp(&a.2))
    });

    // The density ranking is the section's answer, so it leads. The
    // recency-weighted score below it is reference material only — its top slots
    // are routinely lockfiles and CI manifests that dependabot rewrites.
    sub_head(&format!(
        "Fix density: fixes / touches (files with ≥{} touches; {} candidates)",
        DENSITY_MIN_TOUCH as i64,
        ranked.len()
    ));
    let max_density = ranked.first().map(|(d, _, _)| *d).unwrap_or(1.0);
    for (d, k, f) in ranked.iter().take(RISK_TOP) {
        println!(
            "  {d:.2}  {}  fixes {:>3} / touches {:<3}  {}",
            bar(*d, max_density, 10),
            *k as i64,
            touch.get(f) as i64,
            truncate_display(&safe(f), 60)
        );
    }

    // Descriptive only — see the note above on why this is not a test.
    let total_fixes: f64 = score.keys().map(|f| cnt.get(f)).sum();
    let total_touch: f64 = score.keys().map(|f| touch.get(f)).sum();
    let p0 = if total_touch > 0.0 {
        total_fixes / total_touch
    } else {
        0.0
    };
    let above = ranked.iter().filter(|(d, _, _)| *d > p0).count();
    let per_commit = fix_files_per_commit(store, &ranked);
    if let Some(mean) = per_commit {
        println!(
            "  ⓘ {} candidates have a fix rate above the repository average of {:.3}; fix commits touch {:.1} files on average, so no significance judgement is made",
            above, p0, mean
        );
    } else {
        println!(
            "  ⓘ {} candidates have a fix rate above the repository average of {:.3}",
            above, p0
        );
    }

    // Reference only. This is the Bugspots-style recency weighting: a fix last
    // month counts ~0.5, one from the first year of history ~0. It is dominated
    // by lockfiles and CI manifests that dependabot rewrites every release, so it
    // is printed after — never instead of — the density ranking.
    sub_head(
        "Reference: recency-weighted fixes (recent fixes score higher; dependabot often dominates)",
    );
    for (f, s) in score.most_common().into_iter().take(5) {
        println!(
            "  {:>6.2}  {:>4} fixes  {}",
            s,
            cnt.get(&f) as i64,
            truncate_display(&safe(&f), 60)
        );
    }
}

/// Mean number of *candidate* files one fix commit touches.
///
/// This is the number that makes a per-file significance test invalid: fix
/// commits are not independent across files, so a null model of "each file
/// independently at the repository average" under-counts the tail.
fn fix_files_per_commit(store: &SqliteStore, ranked: &[(f64, f64, String)]) -> Option<f64> {
    let cand: HashSet<&str> = ranked.iter().map(|(_, _, f)| f.as_str()).collect();
    let mut per_sha: HashMap<String, HashSet<String>> = HashMap::new();
    {
        let mut s = store
            .conn
            .prepare(
                "SELECT c.sha, f.path_current FROM file_changes f JOIN commits c ON c.sha=f.sha
                 JOIN files_at_head h ON h.path=f.path_current
                 WHERE c.is_merge=0 AND f.is_binary=0 AND rx(?1, c.subject || char(10) || c.body)",
            )
            .unwrap();
        let rows = s
            .query_map(params![FIX_RE], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })
            .unwrap();
        for row in rows {
            let (sha, path) = row.unwrap();
            if cand.contains(path.as_str()) {
                per_sha.entry(sha).or_default().insert(path);
            }
        }
    }
    if per_sha.is_empty() {
        return None;
    }
    Some(per_sha.values().map(|s| s.len()).sum::<usize>() as f64 / per_sha.len() as f64)
}

/// 4/5 acceleration or decline over the last 24 months.
///
/// Buckets on the **committer** date ("use committer date, filter bots;
/// deliberately no time-window limit"). The analyzer must declare which date
/// it uses, so: this one uses `commit_ts`.
///
/// The distinction is not cosmetic. On a rebase- or amend-heavy repository the
/// author date is when the author wrote the change and the committer date is
/// when it actually landed, so they can be months apart and land in different
/// months — measured 14 of 71 months on `amnezia-client` alone. Bucketing by
/// author date attributes a commit to the month it was *written*, which for a
/// long-lived branch understates how active the project was *now*.
///
/// `commit_ts` was already stored (it matches git's `%ct` exactly) and was
/// previously never read.
pub fn audit_trend(store: &SqliteStore) {
    const MONTH_EXPR: &str = "(CAST(strftime('%Y','now') AS INT) - CAST(strftime('%Y',c.commit_ts,'unixepoch') AS INT)) * 12 \
        + (CAST(strftime('%m','now') AS INT) - CAST(strftime('%m',c.commit_ts,'unixepoch') AS INT))";
    let mut commits_by: HashMap<i64, i64> = HashMap::new();
    let mut authors_by: HashMap<i64, HashSet<i64>> = HashMap::new();
    {
        let sql = format!(
            "SELECT {MONTH_EXPR} AS m, COUNT(*) FROM commits c JOIN authors a USING(author_id)
             WHERE c.is_merge=0 AND a.is_bot=0 GROUP BY m"
        );
        let mut s = store.conn.prepare(&sql).unwrap();
        let rows = s
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))
            .unwrap();
        for r in rows {
            let (m, n) = r.unwrap();
            commits_by.insert(m, n);
        }
    }
    // Headcount must be a **union** per month, never a sum: someone active for
    // twelve consecutive months is one person, not twelve.
    {
        let sql = format!(
            "SELECT {MONTH_EXPR} AS m, c.author_id FROM commits c JOIN authors a USING(author_id)
             WHERE c.is_merge=0 AND a.is_bot=0"
        );
        let mut s = store.conn.prepare(&sql).unwrap();
        let rows = s
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))
            .unwrap();
        for r in rows {
            let (m, aid) = r.unwrap();
            authors_by.entry(m).or_default().insert(aid);
        }
    }
    // 23 down to 0 == oldest to newest.
    let rng: Vec<i64> = (0..24).rev().collect();
    let cm: Vec<i64> = rng
        .iter()
        .map(|i| *commits_by.get(i).unwrap_or(&0))
        .collect();
    let ca: Vec<i64> = rng
        .iter()
        .map(|i| authors_by.get(i).map(|s| s.len() as i64).unwrap_or(0))
        .collect();

    let (r12, p12): (i64, i64) = (cm[12..].iter().sum(), cm[..12].iter().sum());
    let a12 = union_len(&authors_by, 0, 12);
    let ap12 = union_len(&authors_by, 12, 24);
    layout::section(
        4,
        "Acceleration or decline",
        "Last 24 months (oldest on the left, newest on the right)",
    );
    let spark_cm = spark(&cm);
    let spark_ca = spark(&ca);
    println!("  Commits  {spark_cm}    last 12 months {r12} / previous 12 months {p12}");
    println!(
        "  Authors  {spark_ca}    last 12 months {a12} people / previous 12 months {ap12} people"
    );
    // The sparkline has no axis, so mark which end is "now".
    let now_at = display_width("  Authors  ") + display_width(&spark_ca) - 1;
    println!("{}↑ now", " ".repeat(now_at));
    if p12 < 20 {
        println!(
            "  → {} (too few commits in the previous 12 months to compare)",
            mk_verdict("na")
        );
        return;
    }
    let ratio = r12 as f64 / p12 as f64;
    let level = if ratio < 0.5 {
        "alert"
    } else if ratio < 0.8 {
        "warn"
    } else {
        "ok"
    };
    println!(
        "  Last 12 months / previous 12 months commit ratio = {ratio:.2} → {}",
        mk_verdict(level)
    );
}

fn union_len(by: &HashMap<i64, HashSet<i64>>, lo: i64, hi: i64) -> i64 {
    let mut u: HashSet<i64> = HashSet::new();
    for i in lo..hi {
        if let Some(s) = by.get(&i) {
            u.extend(s.iter().copied());
        }
    }
    u.len() as i64
}

/// Python: `"▁▂▃▄▅▆▇█"[min(7, int(v / mx * 7.999))] if v else "·"`.
fn spark(vals: &[i64]) -> String {
    let mx = vals.iter().copied().max().unwrap_or(0);
    let mx = if mx == 0 { 1.0 } else { mx as f64 };
    const BLOCKS: [&str; 8] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
    let mut s = String::new();
    for v in vals {
        if *v == 0 {
            s.push('·');
        } else {
            // Python's `int()` truncates toward zero; so does `as`.
            let idx = (*v as f64 / mx * 7.999) as usize;
            s.push_str(BLOCKS[idx.min(7)]);
        }
    }
    s
}

/// 5/5 firefighting frequency — reverts, hotfixes, rollbacks.
pub fn audit_fire(store: &SqliteStore) {
    let mut hits: Vec<(i64, String, i64)> = Vec::new();
    {
        let mut s = store
            .conn
            .prepare(
                "SELECT author_ts, subject, isrevert(subject, body) FROM commits WHERE is_merge=0
                   AND (isrevert(subject, body) OR rx(?1, subject))",
            )
            .unwrap();
        let rows = s
            .query_map(params![FIRE_RE], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })
            .unwrap();
        for r in rows {
            hits.push(r.unwrap());
        }
    }
    let native = hits.iter().filter(|h| h.2 != 0).count();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let n12: i64 = store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM commits WHERE is_merge=0 AND author_ts > ?1",
            params![now - 365 * 86400],
            |r| r.get(0),
        )
        .unwrap();
    let h12 = hits
        .iter()
        .filter(|(ts, _, _)| months_ago(*ts) < 12)
        .count() as i64;
    layout::section(
        5,
        "Firefighting frequency",
        &format!(
            "{} revert / hotfix / rollback commits ({} marked natively by git)",
            hits.len(),
            native
        ),
    );
    if !hits.is_empty() {
        let mut years: Vec<i64> = hits.iter().map(|(ts, _, _)| year_of(*ts)).collect();
        years.sort_unstable();
        years.dedup();
        let counts: Vec<(i64, i64)> = years
            .iter()
            .rev()
            .take(6)
            .rev()
            .map(|y| {
                (
                    *y,
                    hits.iter().filter(|(ts, _, _)| year_of(*ts) == *y).count() as i64,
                )
            })
            .collect();
        let max_y = counts.iter().map(|(_, n)| *n).max().unwrap_or(1).max(1) as f64;
        let line = counts
            .iter()
            .map(|(y, n)| format!("{y} {} {}", hbar(*n as f64, max_y, 6), n))
            .collect::<Vec<_>>()
            .join("   ");
        println!("  By year  {line}");
    }
    let mut sorted = hits.clone();
    // Python: `sorted(hits, key=lambda h: -h[0])[:5]` — stable, so equal
    // timestamps keep query order.
    sorted.sort_by_key(|h| std::cmp::Reverse(h.0));
    for (ts, subj, _) in sorted.iter().take(5) {
        println!(
            "  {}  {}",
            fmt_ts(Some(*ts)),
            truncate_display(&safe(subj), 80)
        );
    }
    if n12 < 30 {
        println!(
            "  → {} (only {n12} commits in the last 12 months; not enough to calculate a rate)",
            mk_verdict("na")
        );
        return;
    }
    // Per thousand commits, so the number scales with repo size.
    let rate = h12 as f64 * 1000.0 / n12 as f64;
    let level = if rate > 30.0 {
        "alert"
    } else if rate > 10.0 {
        "warn"
    } else {
        "ok"
    };
    println!(
        "  Last 12 months: {h12} incidents / {n12} commits = {rate:.1} per 1,000 commits → {}",
        mk_verdict(level)
    );
}

fn year_of(ts: i64) -> i64 {
    civil_from_days(days_from_unix(ts)).0
}

/// Identities judged unattributable whose *name* does not explain why — the
/// ones worth reading before trusting bus factor.
pub fn unattributable_warnings(store: &SqliteStore, n_nomerge: i64) -> Vec<String> {
    let Ok(groups) = store.author_groups() else {
        return Vec::new();
    };
    let re = default_bot_re();
    let mut out = Vec::new();
    for g in groups {
        let is_bot = g["is_bot"].as_bool().unwrap_or(false);
        let commits = g["commits"].as_i64().unwrap_or(0);
        let name = g["name"].as_str().unwrap_or("");
        let email = g["email"].as_str().unwrap_or("");
        let local = email.split('@').next().unwrap_or("");
        if is_bot
            && commits as f64 > n_nomerge as f64 * 0.02
            && !re.is_match(name)
            && !re.is_match(local)
        {
            out.push(format!(
                "⚠ {} commits by \"{} <{}>\" were merged by name alone (email has no distinguishing power, often root@localhost or similar) and excluded as unattributable; otherwise bus factor would be calculated as 1",
                commits,
                safe(name),
                safe(email)
            ));
        }
    }
    out
}

/// Commit count and share of messages too vague for the message-based analyses
/// (3 and 5) to mean anything.
pub fn vague_stats(store: &SqliteStore, n_nomerge: i64) -> (i64, f64) {
    let vague: i64 = store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM commits WHERE is_merge=0 AND (
               length(trim(lower(subject))) <= 10 OR lower(trim(subject)) IN
               ('update','fix','wip','changes','minor'))",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    let d = n_nomerge.max(1) as f64;
    (vague, vague as f64 / d)
}

pub fn pr_suffix_count(store: &SqliteStore) -> i64 {
    store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM commits WHERE is_merge=0 AND subject GLOB '*(#[0-9]*)'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0)
}

pub fn non_merge_count(store: &SqliteStore) -> i64 {
    store
        .conn
        .query_row("SELECT COUNT(*) FROM commits WHERE is_merge=0", [], |r| {
            r.get(0)
        })
        .unwrap_or(0)
}

/// Commits attributed to a real person — the denominator bus factor uses.
pub fn human_non_merge_count(store: &SqliteStore) -> i64 {
    store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM commits c JOIN authors a USING(author_id)
             WHERE c.is_merge=0 AND a.is_bot=0",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_most_common_breaks_ties_by_insertion() {
        let mut c = PyCounter::default();
        c.add("b", 5.0);
        c.add("a", 5.0);
        c.add("c", 9.0);
        assert_eq!(
            c.most_common(),
            vec![
                ("c".to_string(), 9.0),
                ("b".to_string(), 5.0),
                ("a".to_string(), 5.0)
            ]
        );
        assert_eq!(c.get("missing"), 0.0);
        assert_eq!(c.sum(), 19.0);
    }

    #[test]
    #[should_panic(expected = "KeyError")]
    fn index_panics_on_a_missing_key_like_pythons_counter() {
        // Kept as the faithful `Counter.__getitem__`. `audit_bugs` no longer
        // calls it — a churn-less file in the top-8 used to crash the report,
        // and now reads as zero churn instead (see the FIXED note there).
        PyCounter::default().index("nope");
    }

    #[test]
    fn spark_renders_zero_as_a_dot_and_scales_to_the_max() {
        assert_eq!(spark(&[0, 1, 10]), "·▁█");
        assert_eq!(spark(&[5, 5, 5]), "███");
        assert_eq!(spark(&[0, 0]), "··");
    }

    #[test]
    fn union_len_deduplicates_across_months() {
        let mut m: HashMap<i64, HashSet<i64>> = HashMap::new();
        m.insert(0, [1, 2].into_iter().collect());
        m.insert(1, [2, 3].into_iter().collect());
        // 1,2,3 across both months — not 4.
        assert_eq!(union_len(&m, 0, 2), 3);
        assert_eq!(union_len(&m, 2, 4), 0);
    }

    #[test]
    fn verdicts_cover_every_level() {
        assert!(mk_verdict("ok").contains('🟢'));
        assert!(mk_verdict("warn").contains('🟡'));
        assert!(mk_verdict("alert").contains('🔴'));
        assert!(mk_verdict("na").contains('⚪'));
        assert!(mk_verdict("anything else").contains('⚪'));
    }
}
