//! Fingerprints the shader translator, so that cached translations
//! (`src/shader_cache.rs`) last until the translator itself changes and not
//! only until the next build of anything.

use std::path::{Path, PathBuf};

fn files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            files(&path, out);
        } else {
            out.push(path);
        }
    }
}

fn main() {
    // This crate's test executables export the Agility SDK symbols (from
    // victory-shader), so on Windows 10 its GPU tests get the SM 6.6 runtime
    // in `.\D3D12\` beside them.
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        for arg in ["/INCLUDE:D3D12SDKVersion", "/INCLUDE:D3D12SDKPath", "/EXPORT:D3D12SDKVersion,DATA", "/EXPORT:D3D12SDKPath,DATA"] {
            println!("cargo:rustc-link-arg={arg}");
        }
    }
    let here =PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default());
    let crates = here.join("..");
    let root = crates.join("..");
    // The translator, what it is built from, and the form entries are kept in.
    let mut all = vec![
        crates.join("victory-shader/Cargo.toml"),
        here.join("src/shader_cache.rs"),
        root.join("Cargo.lock"),
        root.join("rust-toolchain.toml"),
    ];
    let source = crates.join("victory-shader/src");
    println!("cargo:rerun-if-changed={}", source.display());
    files(&source, &mut all);
    all.sort();
    let mut hash = blake3::Hasher::new();
    for path in &all {
        println!("cargo:rerun-if-changed={}", path.display());
        // Names and line endings as they are in every checkout.
        let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let bytes: Vec<u8> = std::fs::read(path).unwrap_or_default().into_iter().filter(|&b| b != 13).collect();
        hash.update(name.as_bytes());
        hash.update(&(bytes.len() as u64).to_le_bytes());
        hash.update(&bytes);
    }
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rustc-env=VICTORY_TRANSLATOR={}", hash.finalize().to_hex());
}
