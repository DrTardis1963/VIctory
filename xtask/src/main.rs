//! Repository maintenance. Run with `cargo xtask <command>`.

mod agility;
mod gate;
mod harness;
mod hlestub;
#[cfg(test)]
mod hook_tests;
mod json;
mod opcodes;
mod policy;
mod progress;
mod todo;
mod trace;

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
    /// Read HLE call traces (`VICTORY_HLE_TRACE`): `summary` (where each
    /// thread stopped) and `diff` (where two runs part).
    Trace {
        #[command(subcommand)]
        cmd: trace::TraceCmd,
    },
    /// Scaffold an HLE function for `<dll>!<function>` in the module that
    /// serves the DLL: a stub handler, its export, a test. Prints the
    /// patch; --write applies it.
    HleStub {
        dll: String,
        function: String,
        /// Edit the module file instead of printing the patch.
        #[arg(long)]
        write: bool,
        /// The module file, when several serve the DLL.
        #[arg(long)]
        module: Option<PathBuf>,
        /// The modules folder (default: victory-hle's).
        #[arg(long)]
        modules_dir: Option<PathBuf>,
        /// What the stub returns, as Rust code.
        #[arg(long, default_value = "0")]
        returns: String,
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
        Cmd::Agility { from, check } => agility::run(&root, from, check),
        Cmd::Opcodes { from, all, json } => opcodes::run(&root, from, all, json),
        Cmd::Trace { cmd } => trace::run(cmd),
        Cmd::HleStub {
            dll,
            function,
            write,
            module,
            modules_dir,
            returns,
        } => hlestub::run(
            &root,
            &hlestub::Options {
                dll,
                function,
                write,
                module,
                modules_dir,
                returns,
            },
        ),
    }
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives in the workspace")
        .into()
}
