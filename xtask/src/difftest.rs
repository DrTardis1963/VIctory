//! `cargo xtask diff-test`: the shader translator against a reference
//! interpreter (victory-shader's emit/interp.rs).
//!
//! Runs victory-shader's differential tests: the corpus of synthetic GFX10
//! compute programs, and `--cases` random straight-line programs from
//! `--seed`, each run lane by lane on the interpreter and, translated to
//! DXIL, on the host GPU, with the output buffers compared. A mismatch
//! prints the program's disassembly, the word (row and thread) and both
//! values; a random one names its seed, so that `--cases 1 --seed <seed>`
//! reruns it alone. Needs a shader model 6.6 device, so the Agility
//! runtime is installed first. Writes nothing.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, ensure};

pub fn run(root: &Path, cases: u64, seed: u64, ignored: bool) -> Result<()> {
    if let Err(e) = crate::agility::install(root, None, false) {
        eprintln!("diff-test: Agility SDK not installed, the GPU side will be skipped: {e:#}");
    }
    let mut args = vec![
        "test",
        "-p",
        "victory-shader",
        "--lib",
        "--locked",
        "--",
        "emit::interp",
        "--nocapture",
        "--test-threads=1",
    ];
    if ignored {
        // The cases the translation is known to get wrong, too.
        args.push("--include-ignored");
    }
    eprintln!("diff-test: corpus and {cases} random programs from seed {seed}");
    let status = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .args(&args)
        .env("VICTORY_DIFF_CASES", cases.to_string())
        .env("VICTORY_DIFF_SEED", seed.to_string())
        .current_dir(root)
        .status()
        .context("starting cargo test")?;
    ensure!(
        status.success(),
        "the translation differs from the reference (or a test failed); see above"
    );
    Ok(())
}
