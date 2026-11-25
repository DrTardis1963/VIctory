//! Repository maintenance. Run with `cargo xtask <command>`.

mod gate;
mod harness;
mod json;
mod policy;
mod progress;
mod todo;

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "xtask")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Enforce the asset policy: the repository holds only small text files
    /// we wrote. Checks tracked and untracked-but-not-ignored files.
    Policy {
        /// Check only what is staged for commit (used by the pre-commit hook).
        #[arg(long)]
        staged: bool,
    },
    /// Point git at .githooks/ so every commit runs the policy check.
    InstallHooks,
    /// Write the README's progress tracker from docs/progress/progress.json.
    Progress,
    /// Deep clean-room log analyzer, A/B comparator, profile launcher, and shader inspector.
    Harness {
        #[command(subcommand)]
        cmd: harness::HarnessCmd,
    },
    /// The verification gate in one command: policy, clippy, clippy with
    /// every feature, tests. Prints only what failed; full logs go to
    /// target/xtask-gate/.
    Gate {
        /// Check only packages changed since the merge base with `--base`
        /// (and those depending on them).
        #[arg(long)]
        changed: bool,
        /// The branch `--changed` compares with.
        #[arg(long, default_value = "origin/main")]
        base: String,
        /// Run every step even after one fails.
        #[arg(long)]
        keep_going: bool,
        /// Print the summary as one JSON object on stdout.
        #[arg(long)]
        json: bool,
    },
    /// Rank the unimplemented imports titles called (the ledgers
    /// `victory run --execute` writes) and name the HLE module for each.
    Todo {
        /// Ledger files; default: every title's under the data directory.
        files: Vec<PathBuf>,
        /// Show at most this many.
        #[arg(long, default_value_t = 40)]
        limit: usize,
        /// Print as one JSON object.
        #[arg(long)]
        json: bool,
    },
}

fn main() -> Result<()> {
    let root = workspace_root();
    match Cli::parse().command {
        Cmd::Policy { staged } => policy::run(&root, staged),
        Cmd::Progress => progress::run(&root),
        Cmd::Harness { cmd } => harness::run(&root, cmd),
        Cmd::InstallHooks => {
            let status = Command::new("git")
                .args(["config", "core.hooksPath", ".githooks"])
                .current_dir(&root)
                .status()
                .context("running git")?;
            anyhow::ensure!(
                status.success(),
                "git config failed (is this a git repository?)"
            );
            println!("hooks installed: commits now run `cargo xtask policy --staged`");
            Ok(())
        }
        Cmd::Gate {
            changed,
            base,
            keep_going,
            json,
        } => gate::run(
            &root,
            &gate::Options {
                changed,
                keep_going,
                json,
                base,
            },
        ),
        Cmd::Todo { files, limit, json } => todo::run(&root, files, limit, json),
    }
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives in the workspace")
        .into()
}
