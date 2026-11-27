//! `cargo xtask worktrees`: parallel agents, one worktree each, and landing
//! only what passed the gate.
//!
//! - `new <name>`: a worktree `.worktrees/claude-<name>` on a new branch
//!   `claude/<name>` off the base, with the build folder to give its agent.
//! - `status`: every worktree under `.worktrees/`: branch, commits ahead of
//!   and behind the base, uncommitted files, and its last gate result
//!   (`target/xtask-gate/summary.json`) and whether that was for its HEAD.
//! - `land <name>...`: merges into the current branch, one by one, each
//!   worktree whose last full gate passed on its clean HEAD. A merge that
//!   conflicts is aborted and reported. Then it runs the gate here, and with
//!   `--push` fast-forwards the base's branch to the result.
//!
//! The agents themselves are started by the coordinating session (each
//! given its worktree, build folder and brief); this does the git and the
//! bookkeeping, so no unchecked work lands.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};
use clap::Subcommand;
use serde_json::Value;

#[derive(Subcommand)]
pub enum WorktreesCmd {
    /// Make `.worktrees/claude-<name>` on branch `claude/<name>` off the base.
    New {
        name: String,
        #[arg(long, default_value = "unified/main")]
        base: String,
    },
    /// Every worktree's branch, distance from the base and last gate.
    Status {
        #[arg(long, default_value = "unified/main")]
        base: String,
        #[arg(long)]
        json: bool,
    },
    /// Merge the named worktrees' branches here if their gate passed, gate
    /// the result, and with --push fast-forward the base to it.
    Land {
        names: Vec<String>,
        #[arg(long, default_value = "unified/main")]
        base: String,
        /// Push the result to the base's remote branch (fast-forward only).
        #[arg(long)]
        push: bool,
        /// Land even when a worktree's last gate was `--changed` only.
        #[arg(long)]
        allow_changed: bool,
    },
}

pub fn run(root: &Path, cmd: WorktreesCmd) -> Result<()> {
    match cmd {
        WorktreesCmd::New { name, base } => new(root, &name, &base),
        WorktreesCmd::Status { base, json } => status(root, &base, json),
        WorktreesCmd::Land {
            names,
            base,
            push,
            allow_changed,
        } => land(root, &names, &base, push, allow_changed),
    }
}

/// The main checkout: worktrees live under its `.worktrees/`.
fn main_checkout(root: &Path) -> Result<PathBuf> {
    let common = git(
        root,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    Ok(Path::new(common.trim())
        .parent()
        .context("the git directory has no parent")?
        .to_owned())
}

fn new(root: &Path, name: &str, base: &str) -> Result<()> {
    ensure!(
        name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'),
        "a worktree name is letters, digits and dashes"
    );
    let dir = main_checkout(root)?
        .join(".worktrees")
        .join(format!("claude-{name}"));
    ensure!(!dir.exists(), "{} exists already", dir.display());
    let branch = format!("claude/{name}");
    let (remote, _) = base.split_once('/').unwrap_or((base, ""));
    let _ = git(root, &["fetch", "-q", remote]);
    git(
        root,
        &[
            "worktree",
            "add",
            "-b",
            &branch,
            &dir.display().to_string(),
            base,
        ],
    )?;
    println!("worktree   {}", dir.display());
    println!("branch     {branch} (off {base})");
    println!(
        "build dir  CARGO_TARGET_DIR=F:/cargo-targets/{name}  (keep big builds off a full C:)"
    );
    println!(
        "then       cargo xtask agility --from <folder with D3D12Core.dll>, and \
         `cargo xtask gate` before `cargo xtask worktrees land {name}`"
    );
    Ok(())
}

struct Tree {
    path: PathBuf,
    branch: String,
    head: String,
    ahead: usize,
    behind: usize,
    dirty: usize,
    /// The last gate: passed, for which commit, whether `--changed` only.
    gate: Option<(bool, String, bool, bool)>,
}

fn trees(root: &Path, base: &str) -> Result<Vec<Tree>> {
    let list = git(root, &["worktree", "list", "--porcelain"])?;
    let mut out = Vec::new();
    for block in list.split("\n\n") {
        let field = |k: &str| {
            block
                .lines()
                .find_map(|l| l.strip_prefix(k))
                .map(str::to_owned)
        };
        let (Some(path), Some(head)) = (field("worktree "), field("HEAD ")) else {
            continue;
        };
        let path = PathBuf::from(path);
        if !path.components().any(|c| c.as_os_str() == ".worktrees") {
            continue;
        }
        let branch = field("branch refs/heads/").unwrap_or_else(|| "(detached)".into());
        let counts = git(
            &path,
            &[
                "rev-list",
                "--left-right",
                "--count",
                &format!("{base}...HEAD"),
            ],
        )
        .unwrap_or_default();
        let mut n = counts.split_whitespace().map(|n| n.parse().unwrap_or(0));
        let (behind, ahead) = (n.next().unwrap_or(0), n.next().unwrap_or(0));
        let dirty = git(&path, &["status", "--porcelain", "--untracked-files=no"])
            .map_or(0, |s| s.lines().count());
        let gate = std::fs::read_to_string(path.join("target/xtask-gate/summary.json"))
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .map(|v| {
                (
                    v["gate"]["ok"].as_bool().unwrap_or(false),
                    v["head"].as_str().unwrap_or_default().to_owned(),
                    v["dirty"].as_bool().unwrap_or(true),
                    v["changed_only"].as_bool().unwrap_or(true),
                )
            });
        out.push(Tree {
            path,
            branch,
            head,
            ahead,
            behind,
            dirty,
            gate,
        });
    }
    Ok(out)
}

/// What the last gate says about a worktree's HEAD.
fn gate_word(t: &Tree) -> &'static str {
    match &t.gate {
        None => "never",
        Some((_, head, _, _)) if *head != t.head => "stale",
        Some((_, _, true, _)) => "dirty",
        Some((false, ..)) => "FAILED",
        Some((true, _, _, true)) => "ok (changed)",
        Some((true, ..)) => "ok",
    }
}

fn status(root: &Path, base: &str, json: bool) -> Result<()> {
    let trees = trees(root, base)?;
    if json {
        let items: Vec<Value> = trees
            .iter()
            .map(|t| {
                serde_json::json!({
                    "path": t.path, "branch": t.branch, "head": t.head,
                    "ahead": t.ahead, "behind": t.behind, "dirty": t.dirty,
                    "gate": gate_word(t),
                })
            })
            .collect();
        println!("{}", Value::Array(items));
        return Ok(());
    }
    println!(
        "{:<40} {:>6} {:>7} {:>6}  gate",
        "branch", "ahead", "behind", "dirty"
    );
    for t in &trees {
        println!(
            "{:<40} {:>6} {:>7} {:>6}  {}",
            t.branch,
            t.ahead,
            t.behind,
            t.dirty,
            gate_word(t)
        );
    }
    Ok(())
}

fn land(root: &Path, names: &[String], base: &str, push: bool, allow_changed: bool) -> Result<()> {
    ensure!(!names.is_empty(), "name the worktrees to land");
    ensure!(
        git(root, &["status", "--porcelain", "--untracked-files=no"])?
            .trim()
            .is_empty(),
        "this worktree has uncommitted changes; commit them first"
    );
    let trees = trees(root, base)?;
    let mut landed = Vec::new();
    for name in names {
        let wanted = format!("claude/{name}");
        let Some(t) = trees
            .iter()
            .find(|t| t.branch == wanted || t.branch == *name)
        else {
            println!("{name}: no such worktree branch");
            continue;
        };
        let word = gate_word(t);
        if !(word == "ok" || allow_changed && word == "ok (changed)") {
            println!("{name}: not landed, its last gate is {word} (run `cargo xtask gate` there)");
            continue;
        }
        if t.dirty > 0 {
            println!("{name}: not landed, {} uncommitted files", t.dirty);
            continue;
        }
        let merged = Command::new("git")
            .args([
                "merge",
                "--no-edit",
                "--no-ff",
                &format!("refs/heads/{}", t.branch),
            ])
            .current_dir(root)
            .output()?;
        if merged.status.success() {
            println!("{name}: merged ({} commits)", t.ahead);
            landed.push(name.clone());
        } else {
            let conflicts =
                git(root, &["diff", "--name-only", "--diff-filter=U"]).unwrap_or_default();
            git(root, &["merge", "--abort"])?;
            println!(
                "{name}: not landed, merge conflicts in: {}",
                conflicts.split_whitespace().collect::<Vec<_>>().join(", ")
            );
        }
    }
    if landed.is_empty() {
        bail!("nothing landed");
    }
    println!("gate on the merged result...");
    let gate = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .args(["xtask", "gate"])
        .current_dir(root)
        .status()?;
    ensure!(
        gate.success(),
        "the merged result fails the gate; the merges are committed here, not pushed"
    );
    if push {
        let (remote, branch) = base
            .split_once('/')
            .context("--push needs a base like remote/branch")?;
        git(root, &["fetch", "-q", remote])?;
        let ff = Command::new("git")
            .args(["merge-base", "--is-ancestor", base, "HEAD"])
            .current_dir(root)
            .status()?;
        ensure!(
            ff.success(),
            "{base} moved; merge it here and land again (no force pushes)"
        );
        git(
            root,
            &["push", remote, &format!("HEAD:refs/heads/{branch}")],
        )?;
        println!("pushed: {base} is now this HEAD");
    }
    println!("landed: {}", landed.join(", "));
    Ok(())
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(gate: Option<(bool, &str, bool, bool)>) -> Tree {
        Tree {
            path: PathBuf::new(),
            branch: "claude/x".into(),
            head: "abc".into(),
            ahead: 1,
            behind: 0,
            dirty: 0,
            gate: gate.map(|(ok, h, d, c)| (ok, h.into(), d, c)),
        }
    }

    #[test]
    fn only_a_full_clean_pass_on_head_is_ok() {
        assert_eq!(gate_word(&tree(None)), "never");
        assert_eq!(gate_word(&tree(Some((true, "old", false, false)))), "stale");
        assert_eq!(gate_word(&tree(Some((true, "abc", true, false)))), "dirty");
        assert_eq!(
            gate_word(&tree(Some((false, "abc", false, false)))),
            "FAILED"
        );
        assert_eq!(
            gate_word(&tree(Some((true, "abc", false, true)))),
            "ok (changed)"
        );
        assert_eq!(gate_word(&tree(Some((true, "abc", false, false)))), "ok");
    }

    #[test]
    fn this_checkout_is_listed() {
        let root = crate::workspace_root();
        // Only meaningful inside a worktree; the main checkout lists others.
        let trees = trees(&root, "HEAD").unwrap();
        if root.components().any(|c| c.as_os_str() == ".worktrees") {
            assert!(
                trees
                    .iter()
                    .any(|t| t.path == root || root.starts_with(&t.path))
            );
        }
    }
}
