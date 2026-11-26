//! Repository maintenance. Run with `cargo xtask <command>`.

mod agility;
mod bench;
mod corpus;
mod gate;
mod gpu_view;
mod harness;
#[cfg(test)]
mod hook_tests;
mod json;
mod opcodes;
mod policy;
mod progress;
mod todo;
mod worktrees;

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
    /// Parallel agents: make worktrees, show their state and last gate, and
    /// land the ones that passed.
    Worktrees {
        #[command(subcommand)]
        cmd: worktrees::WorktreesCmd,
    },
    /// Run the benchmark tests (release) and compare with a saved run.
    Bench {
        /// Keep this run under NAME (e.g. `main`).
        #[arg(long)]
        save: Option<String>,
        /// Compare with the run saved under NAME.
        #[arg(long)]
        against: Option<String>,
        /// Percent worse that counts as a regression.
        #[arg(long, default_value_t = 5.0)]
        threshold: f64,
        /// Fail when a benchmark regressed.
        #[arg(long)]
        strict: bool,
        /// Print as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Compare two `victory translate --json` reports of the same shaders:
    /// programs fixed and broken, totals by stage and failure reason.
    CorpusDiff {
        before: PathBuf,
        after: PathBuf,
        /// List every program and reason, not the first 20.
        #[arg(long)]
        all: bool,
        /// Print as one JSON object.
        #[arg(long)]
        json: bool,
        /// Fail when any program broke.
        #[arg(long)]
        strict: bool,
    },
    /// GFX10 opcode coverage: which opcodes the decoder knows, the shader
    /// tests lower, and the lowering names. Runs victory-shader's unit tests.
    Opcodes {
        /// A list the tests recorded before (one opcode per line), instead
        /// of running them.
        #[arg(long)]
        from: Option<PathBuf>,
        /// List every opcode, not the first 20 of each kind.
        #[arg(long)]
        all: bool,
        /// Print as one JSON object.
        #[arg(long)]
        json: bool,
    },
    /// Copy the D3D12 Agility SDK runtime (shader model 6.6 on Windows 10)
    /// into every build folder, after checking it is the version the
    /// executables ask for.
    Agility {
        /// The folder holding D3D12Core.dll, outside the repository;
        /// remembered for later runs.
        #[arg(long)]
        from: Option<PathBuf>,
        /// Only report; fail if a build folder lacks a current copy.
        #[arg(long)]
        check: bool,
    },
    /// The verification gate in one command: policy, clippy, clippy with
    /// every feature, tests. Prints only what failed; full logs go to
    /// target/xtask-gate/.
    Gate {
        /// Check only packages changed since the merge base with `--base`
        /// (and those depending on them).
        #[arg(long)]
        changed: bool,
        /// The branch `--changed` compares with; default: the current
        /// branch's upstream, else origin/main.
        #[arg(long, default_value = "")]
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
    /// Write view.html beside a GPU command recording (the frame-<n>.ndjson
    /// files `VICTORY_GPU_RECORD=<dir>` writes): per frame a queue timeline,
    /// command lists as trees of passes, totals; pipelines by draws.
    GpuView {
        /// The recording's folder, outside the repository.
        dir: PathBuf,
    },
}

fn main() -> Result<()> {
    let root = workspace_root();
    match Cli::parse().command {
        Cmd::Policy { staged } => policy::run(&root, staged),
        Cmd::Progress => progress::run(&root),
        Cmd::Harness { cmd } => harness::run(&root, cmd),
        Cmd::Worktrees { cmd } => worktrees::run(&root, cmd),
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
        Cmd::Agility { from, check } => agility::run(&root, from, check),
        Cmd::Opcodes { from, all, json } => opcodes::run(&root, from, all, json),
        Cmd::Bench {
            save,
            against,
            threshold,
            strict,
            json,
        } => bench::run(
            &root,
            &bench::Options {
                save,
                against,
                threshold,
                strict,
                json,
            },
        ),
        Cmd::CorpusDiff {
            before,
            after,
            all,
            json,
            strict,
        } => corpus::run(&before, &after, all, json, strict),
        Cmd::GpuView { dir } => gpu_view::run(&root, &dir),
    }
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives in the workspace")
        .into()
}
