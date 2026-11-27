//! Repository maintenance. Run with `cargo xtask <command>`.

mod agility;
mod bench;
mod corpus;
mod difftest;
mod gate;
mod gpu_view;
mod harness;
mod hlestub;
#[cfg(test)]
mod hook_tests;
mod json;
mod opcodes;
mod policy;
mod progress;
mod shadertest;
mod todo;
mod trace;
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
    /// programs fixed and broken, totals by stage and failure reason. Both
    /// must be made with the same checks and host; scoreboard runs on a
    /// 32-lane GPU (NVIDIA): `translate --validate --pipelines --host-lanes
    /// 32-32 --json <file>`, as without --host-lanes 64-lane programs keep
    /// a wave size such a GPU refuses.
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
        /// Compare reports made for different hosts or checks anyway.
        #[arg(long)]
        any_host: bool,
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
    /// Run synthetic GFX10 programs on a reference interpreter and, translated,
    /// on the GPU, and compare the outputs: the corpus, then random programs.
    DiffTest {
        /// Random straight-line programs to run after the corpus.
        #[arg(long, default_value_t = 200)]
        cases: u64,
        /// The first random program's seed (each next one adds 1).
        #[arg(long, default_value_t = 1)]
        seed: u64,
        /// Also run the known miscompiles (tests marked #[ignore]).
        #[arg(long)]
        ignored: bool,
    },
    /// Lower victory-shader's synthetic snapshot shaders and compare their
    /// `dxc -dumpbin` text with crates/victory-shader/tests/snapshots/.
    Snapshots {
        /// Rewrite the snapshots from the current lowering.
        #[arg(long)]
        bless: bool,
    },
    /// victory-shader's property tests (decoder on random words; random
    /// programs lowered and validated) with more cases; prints failing seeds.
    Fuzz {
        /// Programs per property (the decoder property runs 40 times as many).
        #[arg(long, default_value_t = 5000)]
        cases: u64,
        /// The first seed.
        #[arg(long, default_value_t = 1)]
        seed: u64,
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
            any_host,
        } => corpus::run(&before, &after, all, json, strict, any_host),
        Cmd::GpuView { dir } => gpu_view::run(&root, &dir),
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
        Cmd::DiffTest {
            cases,
            seed,
            ignored,
        } => difftest::run(&root, cases, seed, ignored),
        Cmd::Snapshots { bless } => shadertest::snapshots(&root, bless),
        Cmd::Fuzz { cases, seed } => shadertest::fuzz(&root, cases, seed),
    }
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives in the workspace")
        .into()
}
