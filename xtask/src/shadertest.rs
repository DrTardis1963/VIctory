//! `cargo xtask snapshots` and `cargo xtask fuzz`: victory-shader's DXIL
//! snapshot test and its property tests, run alone with the switches they
//! read from the environment.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

fn shader_tests(root: &Path, filter: &str) -> Command {
    let mut command = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    command
        .args([
            "test",
            "-p",
            "victory-shader",
            "--lib",
            "--locked",
            "--",
            filter,
        ])
        .current_dir(root);
    command
}

/// The golden DXIL test; `bless` rewrites the snapshot files from the
/// current lowering instead of comparing.
pub fn snapshots(root: &Path, bless: bool) -> Result<()> {
    let mut command = shader_tests(root, "emit::snapshot_tests");
    if bless {
        command.env("VICTORY_BLESS", "1").arg("--nocapture");
    }
    let status = command.status().context("starting cargo test")?;
    if !status.success() {
        bail!("the DXIL snapshots differ (or the test failed); `--bless` rewrites them if meant");
    }
    if bless {
        println!(
            "snapshots: rewritten under crates/victory-shader/tests/snapshots/; review the diff"
        );
    }
    Ok(())
}

/// The property tests with `cases` programs from `seed` on; prints each
/// failing seed and its program.
pub fn fuzz(root: &Path, cases: u64, seed: u64) -> Result<()> {
    eprintln!("fuzz: {cases} programs per property from seed {seed}");
    let out = shader_tests(root, "emit::fuzz_tests")
        .arg("--nocapture")
        .env("VICTORY_FUZZ_CASES", cases.to_string())
        .env("VICTORY_FUZZ_SEED", seed.to_string())
        .output()
        .context("starting cargo test")?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    let mut failures = 0;
    let mut lines = stderr.lines().peekable();
    while let Some(line) = lines.next() {
        if line.starts_with("FUZZ FAILURE") {
            failures += 1;
            println!("{line}");
            // The program follows, up to the next failure or test output.
            let program = |l: &&str| {
                !["FUZZ FAILURE", "fuzz:", "thread ", "test "]
                    .iter()
                    .any(|p| l.starts_with(p))
                    && !l.is_empty()
            };
            while let Some(next) = lines.next_if(program) {
                println!("    {next}");
            }
        } else if line.starts_with("fuzz:") {
            println!("{line}");
        }
    }
    if out.status.success() {
        println!("fuzz: no failures");
        return Ok(());
    }
    if failures == 0 {
        // A build error, or a test that failed some other way.
        eprintln!("{stderr}");
        eprintln!("{}", String::from_utf8_lossy(&out.stdout));
    }
    bail!("{failures} failing seed(s); replay one with `cargo xtask fuzz --cases 1 --seed <seed>`")
}
