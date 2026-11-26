//! `cargo xtask snapshots`: victory-shader's DXIL snapshot test, run alone
//! with the switch it reads from the environment.

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
