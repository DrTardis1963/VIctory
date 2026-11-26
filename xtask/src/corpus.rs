//! `cargo xtask corpus-diff <before.json> <after.json>`: what a change did to
//! the title's shaders, from two `victory translate --json` reports.
//!
//! Programs are matched by their key (`#<hash> ~<code> <stage>`), which is
//! the same on every machine. A program is fixed when it failed before and is
//! ok after, broken the other way round; for programs failing in both, a
//! changed first reason is listed too. Totals are given by stage and by
//! failure reason, so a scoreboard reads "fixes 40, breaks 2".

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::Value;

/// How many programs of each list are shown without `--all`.
const SHOWN: usize = 20;

#[derive(Clone, Debug, PartialEq)]
struct Program {
    stage: String,
    status: String,
    reason: Option<String>,
    label: String,
}

#[derive(Debug, Default)]
struct Diff {
    fixed: Vec<(String, Program, Program)>,
    broken: Vec<(String, Program, Program)>,
    changed: Vec<(String, Program, Program)>,
    added: Vec<(String, Program)>,
    removed: Vec<(String, Program)>,
    /// Stage → (ok before, ok after, programs after).
    stages: BTreeMap<String, (usize, usize, usize)>,
    /// First failure reason → (programs before, programs after).
    reasons: BTreeMap<String, (usize, usize)>,
}

pub fn run(
    before: &Path,
    after: &Path,
    all: bool,
    json: bool,
    strict: bool,
    any_host: bool,
) -> Result<()> {
    let (sa, a) = load(before)?;
    let (sb, b) = load(after)?;
    if sa != sb && !any_host {
        bail!(
            "the two reports were made for different hosts or checks, which changes \
             results by itself:\n  before: {sa}\n  after:  {sb}\nMake both with the same \
             flags (scoreboard runs on this machine: `translate --validate --pipelines \
             --host-lanes 32-32 --json <file>`), or pass --any-host"
        );
    }
    let d = diff(&a, &b);
    if json {
        println!("{}", to_json(&d));
    } else {
        print(&d, all);
    }
    if strict && !d.broken.is_empty() {
        bail!("{} programs broke", d.broken.len());
    }
    Ok(())
}

/// What a report's results depend on besides the translator: the checks run
/// and the host translated for (`"unknown"` in reports older than that).
fn settings(report: &Value) -> String {
    let host = &report["host"];
    if host.is_null() {
        return "unknown host".into();
    }
    format!(
        "validate={} pipelines={} max_wave={} rovs={} view_limit={}",
        report["validate"], report["pipelines"], host["max_wave"], host["rovs"], host["view_limit"]
    )
}

fn load(file: &Path) -> Result<(String, BTreeMap<String, Program>)> {
    let text =
        std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    let report: Value =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", file.display()))?;
    let programs = report["programs"].as_array().with_context(|| {
        format!(
            "{} has no programs (made by `victory translate --json`?)",
            file.display()
        )
    })?;
    let mut out = BTreeMap::new();
    for p in programs {
        let field = |k: &str| p[k].as_str().unwrap_or_default().to_owned();
        if field("status") == "duplicate" {
            continue;
        }
        out.insert(
            field("key"),
            Program {
                stage: field("stage"),
                status: field("status"),
                reason: p["reason"].as_str().map(str::to_owned),
                label: field("label"),
            },
        );
    }
    Ok((settings(&report), out))
}

fn diff(a: &BTreeMap<String, Program>, b: &BTreeMap<String, Program>) -> Diff {
    let mut d = Diff::default();
    let ok = |p: &Program| p.status == "ok";
    for (key, pa) in a {
        match b.get(key) {
            None => d.removed.push((key.clone(), pa.clone())),
            Some(pb) if !ok(pa) && ok(pb) => d.fixed.push((key.clone(), pa.clone(), pb.clone())),
            Some(pb) if ok(pa) && !ok(pb) => d.broken.push((key.clone(), pa.clone(), pb.clone())),
            Some(pb) if !ok(pa) && pa.reason != pb.reason => {
                d.changed.push((key.clone(), pa.clone(), pb.clone()))
            }
            Some(_) => {}
        }
    }
    for (key, pb) in b {
        if !a.contains_key(key) {
            d.added.push((key.clone(), pb.clone()));
        }
    }
    for p in a.values() {
        d.stages.entry(p.stage.clone()).or_default().0 += usize::from(ok(p));
        if let Some(r) = &p.reason {
            d.reasons.entry(r.clone()).or_default().0 += 1;
        }
    }
    for p in b.values() {
        let s = d.stages.entry(p.stage.clone()).or_default();
        s.1 += usize::from(ok(p));
        s.2 += 1;
        if let Some(r) = &p.reason {
            d.reasons.entry(r.clone()).or_default().1 += 1;
        }
    }
    d
}

fn print(d: &Diff, all: bool) {
    let (before, after, total) = d
        .stages
        .values()
        .fold((0, 0, 0), |t, s| (t.0 + s.0, t.1 + s.1, t.2 + s.2));
    println!(
        "fixes {}, breaks {}, reason changed {}; ok {before} -> {after} ({:+}) of {total} programs",
        d.fixed.len(),
        d.broken.len(),
        d.changed.len(),
        after as i64 - before as i64,
    );
    if !d.added.is_empty() || !d.removed.is_empty() {
        println!(
            "{} programs only after, {} only before (different corpus or duplicates)",
            d.added.len(),
            d.removed.len()
        );
    }
    println!(
        "\n{:<6} {:>8} {:>8} {:>8} {:>9}",
        "stage", "before", "after", "delta", "programs"
    );
    for (stage, (a, b, n)) in &d.stages {
        println!(
            "{stage:<6} {a:>8} {b:>8} {:>+8} {n:>9}",
            *b as i64 - *a as i64
        );
    }
    let mut reasons: Vec<_> = d.reasons.iter().filter(|(_, (a, b))| a != b).collect();
    reasons.sort_by_key(|(r, (a, b))| (std::cmp::Reverse((*b as i64 - *a as i64).abs()), *r));
    if !reasons.is_empty() {
        println!("\nfailure reasons that changed (programs before -> after):");
        for (reason, (a, b)) in reasons.iter().take(if all { usize::MAX } else { SHOWN }) {
            println!("  {a:>6} -> {b:<6} {:>+6}  {reason}", *b as i64 - *a as i64);
        }
    }
    for (title, list) in [
        ("broken", &d.broken),
        ("fixed", &d.fixed),
        ("reason changed", &d.changed),
    ] {
        if list.is_empty() {
            continue;
        }
        println!("\n{title}: {}", list.len());
        let shown = if all {
            list.len()
        } else {
            SHOWN.min(list.len())
        };
        for (key, pa, pb) in &list[..shown] {
            println!(
                "  {key}\n      {} -> {}",
                pa.reason.as_deref().unwrap_or("ok"),
                pb.reason.as_deref().unwrap_or("ok")
            );
        }
        if list.len() > shown {
            println!("  ... {} more (--all)", list.len() - shown);
        }
    }
}

fn to_json(d: &Diff) -> Value {
    let changes = |list: &[(String, Program, Program)]| -> Value {
        list.iter()
            .map(|(key, a, b)| {
                serde_json::json!({
                    "key": key, "stage": b.stage, "label": b.label,
                    "before": a.reason, "after": b.reason,
                })
            })
            .collect()
    };
    let stages: serde_json::Map<String, Value> = d
        .stages
        .iter()
        .map(|(s, (a, b, n))| {
            (
                s.clone(),
                serde_json::json!({"ok_before": a, "ok_after": b, "programs": n}),
            )
        })
        .collect();
    let reasons: serde_json::Map<String, Value> = d
        .reasons
        .iter()
        .map(|(r, (a, b))| (r.clone(), serde_json::json!({"before": a, "after": b})))
        .collect();
    serde_json::json!({
        "fixed": d.fixed.len(),
        "broken": d.broken.len(),
        "changed": d.changed.len(),
        "only_before": d.removed.len(),
        "only_after": d.added.len(),
        "stages": stages,
        "reasons": reasons,
        "broken_programs": changes(&d.broken),
        "fixed_programs": changes(&d.fixed),
        "changed_programs": changes(&d.changed),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(stage: &str, status: &str, reason: Option<&str>) -> Program {
        Program {
            stage: stage.into(),
            status: status.into(),
            reason: reason.map(Into::into),
            label: String::new(),
        }
    }

    #[test]
    fn programs_are_sorted_into_fixed_broken_and_changed() {
        let a = BTreeMap::from([
            ("#1 ~a Vs".into(), p("Vs", "fail", Some("emit: x"))),
            ("#2 ~b Ps".into(), p("Ps", "ok", None)),
            ("#3 ~c Cs".into(), p("Cs", "fail", Some("structure: y"))),
            ("#4 ~d Cs".into(), p("Cs", "ok", None)),
            ("#5 ~e Ms".into(), p("Ms", "ok", None)),
        ]);
        let b = BTreeMap::from([
            ("#1 ~a Vs".into(), p("Vs", "ok", None)),
            ("#2 ~b Ps".into(), p("Ps", "fail", Some("validator: z"))),
            ("#3 ~c Cs".into(), p("Cs", "fail", Some("emit: w"))),
            ("#4 ~d Cs".into(), p("Cs", "ok", None)),
            ("#6 ~f Ms".into(), p("Ms", "ok", None)),
        ]);
        let d = diff(&a, &b);
        assert_eq!(d.fixed.len(), 1);
        assert_eq!(d.fixed[0].0, "#1 ~a Vs");
        assert_eq!(d.broken[0].0, "#2 ~b Ps");
        assert_eq!(d.changed[0].0, "#3 ~c Cs");
        assert_eq!((d.added.len(), d.removed.len()), (1, 1));
        assert_eq!(d.stages["Cs"], (1, 1, 2));
        assert_eq!(d.stages["Vs"], (0, 1, 1));
        assert_eq!(d.reasons["emit: x"], (1, 0));
        assert_eq!(d.reasons["validator: z"], (0, 1));
        let j = to_json(&d);
        assert_eq!(j["broken"], 1);
        assert_eq!(j["broken_programs"][0]["after"], "validator: z");
    }

    #[test]
    fn reports_are_read_without_duplicates() {
        let dir = std::env::temp_dir().join(format!("xtask-corpus-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("r.json");
        std::fs::write(
            &file,
            r##"{"version":1,"programs":[
                {"key":"#1 ~a Vs","stage":"Vs","status":"ok","reason":null,"label":"x"},
                {"key":"#2 ~a Vs","stage":"Vs","status":"duplicate","reason":null,"label":"y"},
                {"key":"#3 ~c Ps","stage":"Ps","status":"fail","reason":"emit: q","label":"z"}]}"##,
        )
        .unwrap();
        let (settings, got) = load(&file).unwrap();
        assert_eq!(settings, "unknown host");
        assert_eq!(got.len(), 2);
        assert_eq!(got["#3 ~c Ps"].reason.as_deref(), Some("emit: q"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reports_for_different_hosts_differ_in_their_settings() {
        let report = |wave: u32| {
            serde_json::json!({
                "validate": true, "pipelines": true,
                "host": {"max_wave": wave, "lanes": null, "rovs": false, "view_limit": 0},
            })
        };
        assert_eq!(settings(&report(32)), settings(&report(32)));
        assert_ne!(settings(&report(32)), settings(&report(64)));
        assert!(settings(&report(32)).contains("max_wave=32"));
    }
}
