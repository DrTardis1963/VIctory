//! `cargo xtask gate`: the verification gate (AGENTS.md) as one command.
//!
//! Runs, in order: the asset policy, clippy, clippy with every feature, and
//! the tests (the GPU-backed crates' apart, as CLAUDE.md runs them). Each
//! cargo step's full output goes to `target/xtask-gate/`; the summary keeps
//! only the lines that say what failed, so an agent reads a dozen lines
//! instead of a build log. `--changed` checks only the packages changed
//! since the upstream branch (or `--base`) and the packages that depend on
//! them; `--json` prints the summary as one JSON object on stdout.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use anyhow::{Context, Result, bail};

use crate::json;

/// How many lines of a failing step the summary keeps.
const HIGHLIGHTS: usize = 12;

pub struct Options {
    pub changed: bool,
    pub keep_going: bool,
    pub json: bool,
    pub base: String,
}

#[derive(Debug, PartialEq, Eq)]
enum Status {
    Ok,
    Failed,
    Skipped,
}

struct Step {
    name: &'static str,
    status: Status,
    seconds: f64,
    log: Option<PathBuf>,
    highlights: Vec<String>,
}

/// Which packages the cargo steps cover.
#[derive(Debug, PartialEq, Eq)]
enum Scope {
    Workspace,
    Packages(BTreeSet<String>),
}

pub fn run(root: &Path, options: &Options) -> Result<()> {
    let scope = if options.changed {
        changed_scope(root, &options.base)?
    } else {
        Scope::Workspace
    };
    let say = |line: &str| {
        // With --json, stdout is the JSON alone.
        if options.json {
            eprintln!("{line}");
        } else {
            println!("{line}");
        }
    };
    say(&format!("gate: scope {}", describe(&scope)));

    let logs = root.join("target").join("xtask-gate");
    std::fs::create_dir_all(&logs).with_context(|| format!("creating {}", logs.display()))?;

    let mut steps = Vec::new();
    let mut failed = false;

    let started = Instant::now();
    let (status, highlights) = match crate::policy::violations(root, false) {
        Ok((_, v)) if v.is_empty() => (Status::Ok, Vec::new()),
        Ok((_, v)) => (Status::Failed, v.into_iter().take(HIGHLIGHTS).collect()),
        Err(e) => (Status::Failed, vec![format!("{e:#}")]),
    };
    failed |= status == Status::Failed;
    steps.push(Step {
        name: "policy",
        status,
        seconds: started.elapsed().as_secs_f64(),
        log: None,
        highlights,
    });
    say(&line(steps.last().unwrap()));

    let cargo_steps = cargo_steps(&scope);
    // The GPU-backed tests need shader model 6.6, which Windows 10 has only
    // through the Agility SDK runtime beside the test executables.
    if cargo_steps
        .iter()
        .any(|(name, args)| name.starts_with("test-") && !args.is_empty())
    {
        match crate::agility::install(root, None, false) {
            Ok(r) => say(&format!(
                "gate: agility              SDK {} in place ({} folders updated)",
                r.version,
                r.updated.len()
            )),
            Err(e) => say(&format!("gate: agility              not installed: {e:#}")),
        }
    }

    for (name, args) in cargo_steps {
        if args.is_empty() || (failed && !options.keep_going) {
            steps.push(Step {
                name,
                status: Status::Skipped,
                seconds: 0.0,
                log: None,
                highlights: Vec::new(),
            });
            say(&line(steps.last().unwrap()));
            continue;
        }
        let mut command = Command::new(cargo());
        command.args(&args).current_dir(root);
        let log = logs.join(format!("{name}.log"));
        let step = run_cargo(name, command, &log)?;
        failed |= step.status == Status::Failed;
        say(&line(&step));
        steps.push(step);
    }

    if options.json {
        println!("{}", summary_json(&scope, &steps, !failed));
    }
    if failed {
        let names: Vec<_> = steps
            .iter()
            .filter(|s| s.status == Status::Failed)
            .map(|s| s.name)
            .collect();
        bail!("gate failed: {}", names.join(", "));
    }
    say("gate: ok");
    Ok(())
}

/// The crates whose tests run apart, as CLAUDE.md runs them: their unit
/// tests only, victory-gpu's one at a time (they share one device).
const GPU_CRATES: [&str; 2] = ["victory-shader", "victory-gpu"];

/// Each cargo step's name and full arguments; no arguments means the step
/// has nothing in scope and is skipped.
fn cargo_steps(scope: &Scope) -> Vec<(&'static str, Vec<String>)> {
    let strings = |s: &[&str]| s.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let in_scope = |p: &str| match scope {
        Scope::Workspace => true,
        Scope::Packages(set) => set.contains(p),
    };
    let (all, rest): (Vec<String>, Vec<String>) = match scope {
        Scope::Workspace => {
            let mut rest = strings(&["--workspace"]);
            for p in GPU_CRATES {
                rest.extend(strings(&["--exclude", p]));
            }
            (strings(&["--workspace"]), rest)
        }
        Scope::Packages(set) => {
            let flags = |keep: &dyn Fn(&str) -> bool| {
                set.iter()
                    .filter(|p| keep(p))
                    .flat_map(|p| ["-p".to_string(), p.clone()])
                    .collect::<Vec<_>>()
            };
            (flags(&|_| true), flags(&|p| !GPU_CRATES.contains(&p)))
        }
    };
    let with = |head: &[&str], packages: &[String], tail: &[&str]| {
        if packages.is_empty() {
            return Vec::new();
        }
        let mut args = strings(head);
        args.extend_from_slice(packages);
        args.extend(strings(tail));
        args
    };
    let clippy = [
        "clippy",
        "--all-targets",
        "--locked",
        "--message-format=short",
    ];
    let deny = ["--", "-D", "warnings"];
    let mut clippy_all = clippy.to_vec();
    clippy_all.insert(2, "--all-features");
    let gpu = |p: &str, tail: &[&str]| {
        if in_scope(p) {
            with(&["test", "--lib", "--locked"], &strings(&["-p", p]), tail)
        } else {
            Vec::new()
        }
    };
    vec![
        ("clippy", with(&clippy, &all, &deny)),
        ("clippy-all-features", with(&clippy_all, &all, &deny)),
        ("test", with(&["test", "--locked"], &rest, &[])),
        ("test-shader", gpu("victory-shader", &[])),
        ("test-gpu", gpu("victory-gpu", &["--", "--test-threads=1"])),
    ]
}

fn cargo() -> String {
    std::env::var("CARGO").unwrap_or_else(|_| "cargo".into())
}

fn describe(scope: &Scope) -> String {
    match scope {
        Scope::Workspace => "workspace".into(),
        Scope::Packages(p) if p.is_empty() => "no packages changed (policy only)".into(),
        Scope::Packages(p) => p.iter().cloned().collect::<Vec<_>>().join(", "),
    }
}

fn line(step: &Step) -> String {
    let status = match step.status {
        Status::Ok => "ok",
        Status::Failed => "FAILED",
        Status::Skipped => "skipped",
    };
    let mut text = format!("gate: {:<20} {status}", step.name);
    if step.status != Status::Skipped {
        text += &format!(" ({:.1}s)", step.seconds);
    }
    if step.status == Status::Failed {
        if let Some(log) = &step.log {
            text += &format!("  log: {}", log.display());
        }
        for h in &step.highlights {
            text += &format!("\n    {h}");
        }
    }
    text
}

fn run_cargo(name: &'static str, mut command: Command, log: &Path) -> Result<Step> {
    let started = Instant::now();
    let out = command
        .output()
        .with_context(|| format!("starting cargo for {name}"))?;
    let seconds = started.elapsed().as_secs_f64();
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text += &String::from_utf8_lossy(&out.stderr);
    std::fs::write(log, &text).with_context(|| format!("writing {}", log.display()))?;
    let ok = out.status.success();
    Ok(Step {
        name,
        status: if ok { Status::Ok } else { Status::Failed },
        seconds,
        log: Some(log.to_owned()),
        highlights: if ok { Vec::new() } else { highlights(&text) },
    })
}

/// The lines of a failed cargo run that say what failed: compiler and
/// clippy errors, failing tests and their panic messages.
fn highlights(output: &str) -> Vec<String> {
    let mut kept: Vec<String> = Vec::new();
    let mut lines = output.lines().peekable();
    while let Some(line) = lines.next() {
        let trimmed = line.trim_end();
        let keep = trimmed.contains(": error")
            || trimmed.starts_with("error[")
            || (trimmed.starts_with("error:") && !trimmed.starts_with("error: could not compile"))
            || (trimmed.starts_with("test ") && trimmed.ends_with("FAILED"))
            || trimmed.contains("panicked at");
        if !keep {
            continue;
        }
        kept.push(trimmed.to_owned());
        // A panic's message is on the line after `panicked at`.
        if trimmed.contains("panicked at")
            && let Some(next) = lines.peek()
        {
            kept.push(format!("  {}", next.trim_end()));
            lines.next();
        }
    }
    kept.dedup();
    if kept.is_empty() {
        // Something else went wrong (a linker, a timeout): the last lines.
        let all: Vec<&str> = output.lines().collect();
        kept = all[all.len().saturating_sub(HIGHLIGHTS)..]
            .iter()
            .map(|l| l.to_string())
            .collect();
    }
    kept.truncate(HIGHLIGHTS);
    if output.contains("no SM 6.6 device") {
        // Not the change's fault: say so first, before anyone debugs it.
        kept.insert(0, SM66_HINT.into());
    }
    kept
}

/// What a `no SM 6.6 device` failure means.
const SM66_HINT: &str = "hint: no D3D12 device with shader model 6.6, so the GPU-backed tests \
    fail on this machine whatever the change. Windows 10's built-in D3D12 runtime stops at \
    SM 6.5: the test executables must opt into the Agility SDK (export D3D12SDKVersion and \
    D3D12SDKPath) and find D3D12Core.dll in a D3D12 folder beside them.";

fn summary_json(scope: &Scope, steps: &[Step], ok: bool) -> String {
    let scope = match scope {
        Scope::Workspace => json::string("workspace"),
        Scope::Packages(p) => json::array(p.iter().map(|p| json::string(p))),
    };
    let steps = json::array(steps.iter().map(|s| {
        let status = match s.status {
            Status::Ok => "ok",
            Status::Failed => "failed",
            Status::Skipped => "skipped",
        };
        json::object([
            ("name", json::string(s.name)),
            ("status", json::string(status)),
            ("seconds", format!("{:.1}", s.seconds)),
            (
                "log",
                s.log
                    .as_ref()
                    .map_or("null".into(), |l| json::string(&l.display().to_string())),
            ),
            (
                "highlights",
                json::array(s.highlights.iter().map(|h| json::string(h))),
            ),
        ])
    }));
    json::object([("ok", ok.to_string()), ("scope", scope), ("steps", steps)])
}

/// The packages changed since the merge base with `base` (committed,
/// uncommitted or untracked), with every package that depends on them.
/// A change outside any package that cargo reads (the workspace manifest,
/// lock file, toolchain, test fixtures) means the whole workspace.
fn changed_scope(root: &Path, base: &str) -> Result<Scope> {
    let merge_base = if base.is_empty() {
        git(root, &["merge-base", "HEAD", "@{upstream}"])
            .or_else(|_| git(root, &["merge-base", "HEAD", "origin/main"]))?
    } else {
        git(root, &["merge-base", "HEAD", base])?
    };
    let mut files = git(root, &["diff", "--name-only", merge_base.trim()])?;
    files += &git(root, &["ls-files", "--others", "--exclude-standard"])?;
    let members = members(root)?;
    let mut changed = BTreeSet::new();
    for file in files.lines().map(str::trim).filter(|f| !f.is_empty()) {
        match package_of(file, &members) {
            Owner::Package(name) => {
                changed.insert(name);
            }
            Owner::Nothing => {}
            Owner::Everything => return Ok(Scope::Workspace),
        }
    }
    Ok(Scope::Packages(with_dependents(changed, &members)))
}

fn git(root: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .context("running git")?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A workspace member: its folder (relative, `/`-separated) and the
/// workspace packages it depends on, in any dependency table.
struct Member {
    dir: String,
    depends_on: BTreeSet<String>,
}

fn members(root: &Path) -> Result<BTreeMap<String, Member>> {
    let mut members = BTreeMap::new();
    for parent in ["crates", "apps"] {
        let Ok(entries) = std::fs::read_dir(root.join(parent)) else {
            continue;
        };
        for entry in entries.flatten() {
            let dir = format!("{parent}/{}", entry.file_name().to_string_lossy());
            add_member(root, &dir, &mut members)?;
        }
    }
    add_member(root, "xtask", &mut members)?;
    Ok(members)
}

fn add_member(root: &Path, dir: &str, members: &mut BTreeMap<String, Member>) -> Result<()> {
    let manifest = root.join(dir).join("Cargo.toml");
    let Ok(text) = std::fs::read_to_string(&manifest) else {
        return Ok(());
    };
    let table: toml::Table = text
        .parse()
        .with_context(|| format!("parsing {}", manifest.display()))?;
    let Some(name) = table
        .get("package")
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
    else {
        return Ok(());
    };
    let mut depends_on = BTreeSet::new();
    let mut add_deps = |t: &toml::Table| {
        for key in ["dependencies", "dev-dependencies", "build-dependencies"] {
            if let Some(deps) = t.get(key).and_then(|d| d.as_table()) {
                depends_on.extend(deps.keys().cloned());
            }
        }
    };
    add_deps(&table);
    if let Some(targets) = table.get("target").and_then(|t| t.as_table()) {
        for target in targets.values().filter_map(|t| t.as_table()) {
            add_deps(target);
        }
    }
    members.insert(
        name.to_owned(),
        Member {
            dir: dir.to_owned(),
            depends_on,
        },
    );
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum Owner {
    Package(String),
    /// Read by no build (documentation).
    Nothing,
    /// Read by every build, or by tests of unknown packages.
    Everything,
}

fn package_of(file: &str, members: &BTreeMap<String, Member>) -> Owner {
    let file = file.replace('\\', "/");
    if let Some((name, _)) = members
        .iter()
        .find(|(_, m)| file.starts_with(&format!("{}/", m.dir)))
    {
        return Owner::Package(name.clone());
    }
    let documentation = file.ends_with(".md") || file.starts_with("docs/");
    // Hooks and CI do not change what the gate builds.
    let tooling = file.starts_with(".githooks/") || file.starts_with(".github/");
    let ignored = matches!(
        file.as_str(),
        ".gitignore" | ".gitattributes" | "LICENSE" | "victory.example.toml"
    );
    if documentation || tooling || ignored {
        Owner::Nothing
    } else {
        Owner::Everything
    }
}

/// `changed` and every member that depends on one of them, transitively.
fn with_dependents(
    mut changed: BTreeSet<String>,
    members: &BTreeMap<String, Member>,
) -> BTreeSet<String> {
    loop {
        let more: Vec<String> = members
            .iter()
            .filter(|(name, m)| {
                !changed.contains(*name) && m.depends_on.iter().any(|d| changed.contains(d))
            })
            .map(|(name, _)| name.clone())
            .collect();
        if more.is_empty() {
            return changed;
        }
        changed.extend(more);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(dir: &str, deps: &[&str]) -> Member {
        Member {
            dir: dir.into(),
            depends_on: deps.iter().map(|d| d.to_string()).collect(),
        }
    }

    fn synthetic_members() -> BTreeMap<String, Member> {
        BTreeMap::from([
            ("core".into(), member("crates/core", &["anyhow"])),
            ("loader".into(), member("crates/loader", &["core"])),
            ("app".into(), member("apps/app", &["loader"])),
            ("xtask".into(), member("xtask", &["anyhow"])),
        ])
    }

    #[test]
    fn files_map_to_their_package_docs_to_nothing_the_rest_to_everything() {
        let m = synthetic_members();
        assert_eq!(
            package_of("crates/loader/src/lib.rs", &m),
            Owner::Package("loader".into())
        );
        assert_eq!(package_of("docs/architecture.md", &m), Owner::Nothing);
        assert_eq!(package_of("README.md", &m), Owner::Nothing);
        assert_eq!(package_of("Cargo.lock", &m), Owner::Everything);
        assert_eq!(
            package_of("tests/fixtures/x/src/main.rs", &m),
            Owner::Everything
        );
        // A folder whose name only starts like a member's is not that member.
        assert_eq!(
            package_of("crates/loader2/src/lib.rs", &m),
            Owner::Everything
        );
    }

    #[test]
    fn dependents_are_included_transitively() {
        let m = synthetic_members();
        let got = with_dependents(BTreeSet::from(["core".into()]), &m);
        let want: BTreeSet<String> = ["app", "core", "loader"].map(String::from).into();
        assert_eq!(got, want);
        let got = with_dependents(BTreeSet::from(["xtask".into()]), &m);
        assert_eq!(got, BTreeSet::from(["xtask".into()]));
    }

    #[test]
    fn gpu_crates_are_tested_apart_and_only_when_in_scope() {
        let joined = |scope: &Scope| -> Vec<(&str, String)> {
            cargo_steps(scope)
                .into_iter()
                .map(|(name, args)| (name, args.join(" ")))
                .collect()
        };
        let all = joined(&Scope::Workspace);
        assert_eq!(
            all[2].1,
            "test --locked --workspace --exclude victory-shader --exclude victory-gpu"
        );
        assert_eq!(all[3].1, "test --lib --locked -p victory-shader");
        assert_eq!(
            all[4].1,
            "test --lib --locked -p victory-gpu -- --test-threads=1"
        );
        assert!(all[1].1.starts_with("clippy --all-targets --all-features"));
        assert!(all[0].1.ends_with("--workspace -- -D warnings"));

        let some = joined(&Scope::Packages(BTreeSet::from([
            "victory-gpu".into(),
            "xtask".into(),
        ])));
        assert_eq!(
            some[0].1,
            "clippy --all-targets --locked --message-format=short -p victory-gpu -p xtask -- -D warnings"
        );
        assert_eq!(some[2].1, "test --locked -p xtask");
        assert_eq!(some[3].1, "", "victory-shader is not in scope");
        assert!(some[4].1.contains("-p victory-gpu"));
        assert!(
            joined(&Scope::Packages(BTreeSet::new()))
                .iter()
                .all(|(_, a)| a.is_empty())
        );
    }

    #[test]
    fn highlights_keep_errors_failures_and_panic_messages() {
        let output = "\
   Compiling core v0.1.0
crates/core/src/lib.rs:3:5: error: unused variable: `x`
error: could not compile `core` (lib) due to 1 previous error
test tests::fine ... ok
test tests::broken ... FAILED
thread 'tests::broken' panicked at crates/core/src/lib.rs:9:5:
assertion failed: false
test result: FAILED. 1 passed; 1 failed
";
        assert_eq!(
            highlights(output),
            [
                "crates/core/src/lib.rs:3:5: error: unused variable: `x`",
                "test tests::broken ... FAILED",
                "thread 'tests::broken' panicked at crates/core/src/lib.rs:9:5:",
                "  assertion failed: false",
            ]
        );
    }

    #[test]
    fn a_missing_sm66_device_is_explained_first() {
        let output = "\
test emit::a ... FAILED
thread 'emit::a' panicked at crates/s/src/lib.rs:5:5:
no SM 6.6 device
";
        let got = highlights(output);
        assert_eq!(got[0], SM66_HINT);
        assert_eq!(got[1], "test emit::a ... FAILED");
    }

    #[test]
    fn without_recognised_lines_highlights_are_the_tail() {
        let output = (1..=20).map(|i| format!("line {i}\n")).collect::<String>();
        let got = highlights(&output);
        assert_eq!(got.len(), HIGHLIGHTS);
        assert_eq!(got.last().map(String::as_str), Some("line 20"));
    }

    #[test]
    fn the_summary_is_json() {
        let steps = [Step {
            name: "test",
            status: Status::Failed,
            seconds: 1.3,
            log: Some(PathBuf::from(r"C:\t\test.log")),
            highlights: vec!["say \"hi\"".into()],
        }];
        let scope = Scope::Packages(BTreeSet::from(["core".into()]));
        assert_eq!(
            summary_json(&scope, &steps, false),
            r#"{"ok":false,"scope":["core"],"steps":[{"name":"test","status":"failed","seconds":1.3,"log":"C:\\t\\test.log","highlights":["say \"hi\""]}]}"#
        );
    }
}
