//! Terminal report: the header, the data-quality gates, then the five
//! analyses.
//!
//! The gates print **before** any conclusion on purpose — a reader who sees
//! "🔴 Alert" without having read "70% of commit messages are too vague" would
//! draw the wrong conclusion. Python built one list and printed it before
//! invoking the analyzers; this keeps that order explicit.

use crate::analyzers::{
    audit_bugs, audit_bus, audit_churn, audit_fire, audit_trend, human_non_merge_count,
    non_merge_count, pr_suffix_count, unattributable_warnings, vague_stats,
};
use crate::index::IndexResult;
use crate::report::layout::{footer, header_block, sub_head};
use crate::util::clock::fmt_ts;
use crate::util::text::safe;

/// The short name of what is being audited: `sing-box`, not
/// `/Users/lu/repo_audit/proxyServer/sing-box`.
fn repo_label(display: &str) -> String {
    let d = display.trim_end_matches('/');
    let base = d.rsplit('/').next().unwrap_or(d);
    let base = base.strip_suffix(".git").unwrap_or(base);
    if base.is_empty() {
        d.to_string()
    } else {
        base.to_string()
    }
}

pub fn render_report(res: &IndexResult) {
    let store = &res.store;
    let s = store.summary().unwrap();
    let n_nomerge = non_merge_count(store);
    let bots = n_nomerge - human_non_merge_count(store);

    println!(
        "{}",
        header_block(&[
            ("Repository", repo_label(&res.display)),
            (
                "Commits",
                format!(
                    "{} total ({} non-merge, {} bot/unattributable)",
                    s.commits, n_nomerge, bots
                )
            ),
            (
                "Time span",
                format!("{} → {}", fmt_ts(s.first_ts), fmt_ts(s.last_ts))
            ),
        ])
    );

    // Data-quality gates, grouped and before any conclusion.
    let mut gates: Vec<String> = Vec::new();
    for w in unattributable_warnings(store, n_nomerge) {
        gates.push(w);
    }
    let (vague, vague_pct) = vague_stats(store, n_nomerge);
    let pr = pr_suffix_count(store);
    if res.shallow {
        gates.push(
            "⚠ Shallow clone: incomplete history; time-series conclusions are unreliable"
                .to_string(),
        );
    }
    if vague_pct > 0.3 {
        gates.push(format!(
            "⚠ {:.0}% ({}/{}) of commit messages are too vague; items 3 and 5, which rely on commit messages, are unreliable",
            vague as f64 * 100.0 / n_nomerge.max(1) as f64,
            vague,
            n_nomerge
        ));
    }
    if pr as f64 / n_nomerge.max(1) as f64 > 0.5 {
        gates.push(
            "⚠ This looks like a squash workflow (one commit per PR): author and file statistics are measured per PR and will be underestimated"
                .to_string(),
        );
    }
    for w in res.warnings.iter().take(3) {
        gates.push(format!("⚠ {}", safe(w)));
    }
    if !gates.is_empty() {
        sub_head("Data quality: read these warnings before the conclusions");
        for g in gates {
            println!("  {g}");
        }
    }

    let (_churn, touch) = audit_churn(store);
    audit_bus(store);
    audit_bugs(store, &touch);
    audit_trend(store);
    audit_fire(store);
    footer(&[
        "All thresholds above are empirical and have not been calibrated for repository or team size.",
        "Items 3 and 5 depend on commit messages; squash workflows systematically underestimate them.",
        "Item 3 gives rankings, not 🟢/🟡/🔴: fix commits span files (1.3-4.8 on average),",
        "so the usual null hypothesis is invalid; any concentration threshold would flag every repository.",
    ]);
}
