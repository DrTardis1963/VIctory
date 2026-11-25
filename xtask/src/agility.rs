//! `cargo xtask agility`: puts the D3D12 Agility SDK runtime where the
//! executables cargo builds look for it.
//!
//! Windows 10's own D3D12 stops at shader model 6.5; the title's shaders and
//! the GPU tests need 6.6. Executables that export `D3D12SDKVersion` and
//! `D3D12SDKPath` (set in victory-shader) load the runtime from a `D3D12`
//! folder beside them instead, but only one of exactly that version; any
//! other and Windows silently keeps its own. The runtime is Microsoft's, so
//! it never enters the repository: it is copied from a folder outside it
//! into the build directories (under `target/`, ignored by git) only.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};

/// The runtime's files; the first is required.
const FILES: [&str; 2] = ["D3D12Core.dll", "d3d12SDKLayers.dll"];

/// Where the source folder given last is kept, so later runs (and the gate)
/// need no `--from`.
const REMEMBERED: &str = "agility-source.txt";

/// What `run` found and did.
pub struct Report {
    pub version: u32,
    pub source: PathBuf,
    /// Build folders that were given (or brought up to date) a copy.
    pub updated: Vec<PathBuf>,
    /// Build folders whose copy was already current.
    pub current: Vec<PathBuf>,
}

pub fn run(root: &Path, from: Option<PathBuf>, check: bool) -> Result<()> {
    let report = install(root, from, check)?;
    println!(
        "agility: SDK {} from {}",
        report.version,
        report.source.display()
    );
    for dir in &report.current {
        println!("  current  {}", dir.display());
    }
    for dir in &report.updated {
        let verb = if check { "STALE" } else { "copied" };
        println!("  {verb:<8} {}", dir.display());
    }
    if check && !report.updated.is_empty() {
        bail!("run `cargo xtask agility` to copy the runtime");
    }
    Ok(())
}

/// Checks the runtime against the version the executables ask for and
/// copies it into every build folder (only reports, with `check`).
pub fn install(root: &Path, from: Option<PathBuf>, check: bool) -> Result<Report> {
    let target = target_dir(root);
    let source = source_dir(root, &target, from)?;
    let wanted = wanted_version(root)?;
    let core = source.join(FILES[0]);
    let bytes = std::fs::read(&core).with_context(|| format!("reading {}", core.display()))?;
    let found = runtime_version(&bytes)
        .with_context(|| format!("{} is not an Agility SDK runtime", core.display()))?;
    ensure!(
        found == wanted,
        "{} is Agility SDK {found}, but the executables ask for {wanted} \
         (D3D12SDKVersion in crates/victory-shader/src/lib.rs); Windows would \
         ignore it and stay at shader model 6.5",
        core.display()
    );
    if !check {
        std::fs::create_dir_all(&target)?;
        std::fs::write(target.join(REMEMBERED), source.display().to_string())?;
    }

    let mut report = Report {
        version: found,
        source: source.clone(),
        updated: Vec::new(),
        current: Vec::new(),
    };
    for dir in destinations(&target)? {
        if current(&source, &dir) {
            report.current.push(dir);
            continue;
        }
        if !check {
            std::fs::create_dir_all(&dir)?;
            for file in FILES {
                let from = source.join(file);
                if from.is_file() {
                    std::fs::copy(&from, dir.join(file)).with_context(|| {
                        format!("copying {} to {}", from.display(), dir.display())
                    })?;
                }
            }
        }
        report.updated.push(dir);
    }
    Ok(report)
}

fn target_dir(root: &Path) -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR").map_or_else(|| root.join("target"), PathBuf::from)
}

/// The folder holding the runtime: `--from`, else `VICTORY_AGILITY_DIR`,
/// else the one given last, else `D3D12` beside the file `VICTORY_CONFIG`
/// names. It must be outside the repository.
fn source_dir(root: &Path, target: &Path, from: Option<PathBuf>) -> Result<PathBuf> {
    let remembered = std::fs::read_to_string(target.join(REMEMBERED))
        .ok()
        .map(|s| PathBuf::from(s.trim()));
    let beside_config = std::env::var_os("VICTORY_CONFIG")
        .map(PathBuf::from)
        .and_then(|c| c.parent().map(|p| p.join("D3D12")));
    let dir = from
        .or_else(|| std::env::var_os("VICTORY_AGILITY_DIR").map(PathBuf::from))
        .or(remembered)
        .or(beside_config)
        .context(
            "where is the Agility SDK runtime? Pass `--from <folder holding D3D12Core.dll>` \
             once (it is remembered), or set VICTORY_AGILITY_DIR",
        )?;
    let dir = std::path::absolute(&dir).unwrap_or(dir);
    ensure!(
        !dir.starts_with(root),
        "{} is inside the repository; the runtime is Microsoft's and must stay outside it",
        dir.display()
    );
    ensure!(
        dir.join(FILES[0]).is_file(),
        "{} has no {}",
        dir.display(),
        FILES[0]
    );
    Ok(dir)
}

/// The `D3D12SDKVersion` the executables export, read from victory-shader.
fn wanted_version(root: &Path) -> Result<u32> {
    let file = root.join("crates/victory-shader/src/lib.rs");
    let text =
        std::fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
    version_in_source(&text).with_context(|| {
        format!(
            "{} exports no D3D12SDKVersion; this branch does not opt into the Agility SDK",
            file.display()
        )
    })
}

fn version_in_source(text: &str) -> Option<u32> {
    let after = text.split("static D3D12SDKVersion: u32 =").nth(1)?;
    after.split(';').next()?.trim().parse().ok()
}

/// The SDK version a `D3D12Core.dll` exports as `D3D12SDKVersion`.
fn runtime_version(dll: &[u8]) -> Result<u32> {
    let image = victory_pe::ImageInfo::parse(dll)?;
    let rva = image
        .exports
        .iter()
        .find(|e| e.name.as_deref() == Some("D3D12SDKVersion"))
        .and_then(|e| e.rva)
        .context("no D3D12SDKVersion export")?;
    let at = image
        .rva_to_file_offset(rva)
        .context("D3D12SDKVersion lies outside the file")? as usize;
    let bytes = dll
        .get(at..at + 4)
        .context("D3D12SDKVersion lies outside the file")?;
    Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
}

/// `<profile>/D3D12` (for the executables) and `<profile>/deps/D3D12` (for
/// the test executables) in every build profile folder, `debug` always.
fn destinations(target: &Path) -> Result<Vec<PathBuf>> {
    let mut profiles = vec![target.join("debug")];
    if let Ok(entries) = std::fs::read_dir(target) {
        for entry in entries {
            let path = entry?.path();
            if path.join("deps").is_dir() && !profiles.contains(&path) {
                profiles.push(path);
            }
        }
    }
    profiles.sort();
    Ok(profiles
        .into_iter()
        .flat_map(|p| [p.join("D3D12"), p.join("deps").join("D3D12")])
        .collect())
}

/// Whether `dir` already has every file of the runtime, of the same size and
/// time.
fn current(source: &Path, dir: &Path) -> bool {
    FILES.iter().all(|file| {
        let Ok(want) = std::fs::metadata(source.join(file)) else {
            return true; // an optional file the source lacks
        };
        std::fs::metadata(dir.join(file)).is_ok_and(|have| {
            have.len() == want.len() && have.modified().ok() == want.modified().ok()
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wanted_version_is_read_from_the_source() {
        let text = "pub static D3D12SDKVersion: u32 = 614;\npub static D3D12SDKPath";
        assert_eq!(version_in_source(text), Some(614));
        assert_eq!(version_in_source("fn main() {}"), None);
    }

    #[test]
    fn every_profile_gets_both_folders_and_copies_are_detected_current() {
        let tmp = std::env::temp_dir().join(format!("xtask-agility-{}", std::process::id()));
        let target = tmp.join("target");
        let source = tmp.join("sdk");
        std::fs::create_dir_all(target.join("diag/deps")).unwrap();
        std::fs::create_dir_all(target.join("xtask-gate")).unwrap();
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join(FILES[0]), b"core").unwrap();

        let got = destinations(&target).unwrap();
        let want = [
            target.join("debug/D3D12"),
            target.join("debug/deps/D3D12"),
            target.join("diag/D3D12"),
            target.join("diag/deps/D3D12"),
        ];
        assert_eq!(got, want);

        let dir = &want[1];
        assert!(!current(&source, dir));
        std::fs::create_dir_all(dir).unwrap();
        std::fs::copy(source.join(FILES[0]), dir.join(FILES[0])).unwrap();
        assert!(current(&source, dir), "copy keeps size and time");
        std::fs::write(source.join(FILES[0]), b"newer core").unwrap();
        assert!(!current(&source, dir));

        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn a_source_inside_the_repository_is_refused() {
        let root = crate::workspace_root();
        let err = source_dir(&root, &root.join("target"), Some(root.join("xtask"))).unwrap_err();
        assert!(err.to_string().contains("inside the repository"), "{err}");
    }
}
