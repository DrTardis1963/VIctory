//! `cargo xtask bench`: runs the benchmark tests and compares them with a
//! saved run.
//!
//! A benchmark is a test marked `#[ignore = "benchmark"]` whose name ends in
//! `_throughput` and that prints `BENCH <name> <value> <unit>` lines (units
//! ending in `/s` are better higher, the rest lower). They run in release
//! builds. `--save main` on main, then `--against main` on a branch, gives
//! the bench diff; every run is also kept as `last`. Results go to
//! target/xtask-bench/.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

pub struct Options {
    pub save: Option<String>,
    pub against: Option<String>,
    /// A change worse than this many percent is a regression.
    pub threshold: f64,
    pub strict: bool,
    pub json: bool,
}

type Results = BTreeMap<String, (f64, String)>;

pub fn run(root: &Path, o: &Options) -> Result<()> {
    let packages = bench_packages(root)?;
    if packages.is_empty() {
        bail!("no `#[ignore = \"benchmark\"]` tests found");
    }
    eprintln!("bench: {} (release build)", packages.join(", "));
    let mut cmd = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    cmd.args(["test", "--release", "--locked", "-q"]);
    for p in &packages {
        cmd.args(["-p", p]);
    }
    cmd.args([
        "--",
        "--ignored",
        "--nocapture",
        "--test-threads=1",
        "_throughput",
    ]);
    let out = cmd
        .current_dir(root)
        .output()
        .context("running cargo test")?;
    let text = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() {
        bail!(
            "the benchmarks failed:\n{}{}",
            text,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let now = parse(&text);
    if now.is_empty() {
        bail!("the benchmarks printed no BENCH lines");
    }

    let dir = root.join("target").join("xtask-bench");
    std::fs::create_dir_all(&dir)?;
    save(&dir.join("last.json"), &now)?;
    if let Some(name) = &o.save {
        save(&file(&dir, name), &now)?;
        eprintln!("bench: saved as {name}");
    }
    let base = match &o.against {
        Some(name) => Some(load(&file(&dir, name)).with_context(|| {
            format!("no saved run `{name}`; make one with `cargo xtask bench --save {name}`")
        })?),
        None => None,
    };
    let rows = compare(&now, base.as_ref(), o.threshold);
    if o.json {
        let items = rows.iter().map(|r| {
            crate::json::object([
                ("name", crate::json::string(&r.name)),
                ("unit", crate::json::string(&r.unit)),
                ("now", r.now.to_string()),
                ("base", r.base.map_or("null".into(), |b| b.to_string())),
                (
                    "change_percent",
                    r.change.map_or("null".into(), |c| format!("{c:.2}")),
                ),
                ("regressed", r.regressed.to_string()),
            ])
        });
        println!("{}", crate::json::array(items));
    } else {
        println!(
            "{:<34} {:>12} {:>12} {:>9}  unit",
            "benchmark", "base", "now", "change"
        );
        for r in &rows {
            let base = r.base.map_or("-".into(), |b| format!("{b:.1}"));
            let change = r.change.map_or("".into(), |c| format!("{c:+.1}%"));
            let flag = if r.regressed { "  REGRESSED" } else { "" };
            println!(
                "{:<34} {base:>12} {:>12.1} {change:>9}  {}{flag}",
                r.name, r.now, r.unit
            );
        }
    }
    let regressed = rows.iter().filter(|r| r.regressed).count();
    if o.strict && regressed > 0 {
        bail!(
            "{regressed} benchmarks regressed by more than {}%",
            o.threshold
        );
    }
    Ok(())
}

fn file(dir: &Path, name: &str) -> PathBuf {
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    dir.join(format!("{safe}.json"))
}

/// The workspace packages with benchmark tests.
fn bench_packages(root: &Path) -> Result<Vec<String>> {
    let mut found = Vec::new();
    for dir in ["crates", "apps"] {
        let Ok(entries) = std::fs::read_dir(root.join(dir)) else {
            continue;
        };
        for entry in entries {
            let crate_dir = entry?.path();
            let has = walkdir::WalkDir::new(&crate_dir)
                .into_iter()
                .filter_map(Result::ok)
                .filter(|e| e.path().extension().is_some_and(|x| x == "rs"))
                .any(|e| {
                    std::fs::read_to_string(e.path())
                        .is_ok_and(|t| t.contains("#[ignore = \"benchmark\"]"))
                });
            if has {
                let manifest = std::fs::read_to_string(crate_dir.join("Cargo.toml"))?;
                let name = manifest
                    .lines()
                    .find_map(|l| l.strip_prefix("name = "))
                    .map(|n| n.trim().trim_matches('"').to_owned())
                    .context("a crate without a name")?;
                found.push(name);
            }
        }
    }
    found.sort();
    Ok(found)
}

fn parse(output: &str) -> Results {
    output
        .lines()
        .filter_map(|l| {
            let mut words = l.split_once("BENCH ")?.1.split_whitespace();
            let name = words.next()?.to_owned();
            let value: f64 = words.next()?.parse().ok()?;
            let unit = words.next().unwrap_or("").to_owned();
            Some((name, (value, unit)))
        })
        .collect()
}

fn save(file: &Path, results: &Results) -> Result<()> {
    let text: String = results
        .iter()
        .map(|(name, (value, unit))| format!("BENCH {name} {value} {unit}\n"))
        .collect();
    std::fs::write(file, text).with_context(|| format!("writing {}", file.display()))
}

fn load(file: &Path) -> Result<Results> {
    Ok(parse(&std::fs::read_to_string(file)?))
}

struct Row {
    name: String,
    unit: String,
    now: f64,
    base: Option<f64>,
    /// Percent, positive when better.
    change: Option<f64>,
    regressed: bool,
}

fn compare(now: &Results, base: Option<&Results>, threshold: f64) -> Vec<Row> {
    now.iter()
        .map(|(name, (value, unit))| {
            let b = base.and_then(|b| b.get(name)).map(|(v, _)| *v);
            let higher_better = unit.ends_with("/s");
            let change = b.filter(|b| *b > 0.0).map(|b| {
                let raw = (value - b) / b * 100.0;
                if higher_better { raw } else { -raw }
            });
            Row {
                name: name.clone(),
                unit: unit.clone(),
                now: *value,
                base: b,
                change,
                regressed: change.is_some_and(|c| c < -threshold),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bench_lines_are_parsed_and_direction_follows_the_unit() {
        let base = parse("BENCH detile/a 1000.0 MB/s\nnoise\nBENCH bcpack/bc1 200 ns/block\n");
        let now = parse(
            "test x ... BENCH detile/a 900 MB/s\nBENCH bcpack/bc1 180 ns/block\nBENCH new 1 us/frame\n",
        );
        let rows = compare(&now, Some(&base), 5.0);
        let get = |n: &str| rows.iter().find(|r| r.name == n).unwrap();
        assert!(get("detile/a").regressed, "10% fewer MB/s is worse");
        assert!((get("detile/a").change.unwrap() + 10.0).abs() < 1e-9);
        assert!(!get("bcpack/bc1").regressed, "fewer ns is better");
        assert!((get("bcpack/bc1").change.unwrap() - 10.0).abs() < 1e-9);
        assert_eq!(get("new").base, None);
    }

    #[test]
    fn saved_runs_read_back() {
        let dir = std::env::temp_dir().join(format!("xtask-bench-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let r = parse("BENCH a/b 12.5 MB/s\n");
        let f = file(&dir, "main");
        save(&f, &r).unwrap();
        assert_eq!(load(&f).unwrap(), r);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_formats_benchmarks_are_found() {
        let p = bench_packages(&crate::workspace_root()).unwrap();
        assert!(p.contains(&"victory-formats".to_owned()), "{p:?}");
    }
}
