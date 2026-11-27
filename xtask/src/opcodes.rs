//! `cargo xtask opcodes`: how much of GFX10 the shader translator covers.
//!
//! For every opcode in the decoder's tables (crates/victory-shader/src/
//! gfx10/opcodes.rs) it says whether victory-shader's unit tests lowered it
//! (recorded by the lowering itself in test builds, see emit/coverage.rs),
//! else whether the lowering's source names it (a string or a prefix it
//! matches), else neither. Untested opcodes the lowering names are the
//! cheapest tests to add; ones it does not name are refused or handled only
//! generically.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::json;

/// How many names of each list are shown without `--all`.
const SHOWN: usize = 20;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Cover {
    Tested,
    Named,
    Neither,
}

struct Opcode {
    encoding: String,
    name: String,
    cover: Cover,
}

pub fn run(root: &Path, from: Option<PathBuf>, all: bool, as_json: bool) -> Result<()> {
    let shader = root.join("crates/victory-shader/src");
    let table = std::fs::read_to_string(shader.join("gfx10/opcodes.rs"))
        .context("reading the decoder's opcode tables")?;
    let decoded = parse_tables(&table);
    if decoded.is_empty() {
        bail!("found no opcode tables in gfx10/opcodes.rs");
    }
    let (literals, prefixes) = names_in_lowering(&shader)?;
    let tested = match from {
        Some(file) => read_list(&file)?,
        None => run_tests(root)?,
    };

    let opcodes: Vec<Opcode> = decoded
        .into_iter()
        .map(|(encoding, name)| {
            let cover = if tested.contains(&name) {
                Cover::Tested
            } else if literals.contains(&name) || prefixes.iter().any(|p| name.starts_with(p)) {
                Cover::Named
            } else {
                Cover::Neither
            };
            Opcode {
                encoding,
                name,
                cover,
            }
        })
        .collect();

    // encoding -> [decoded, tested, named, neither]
    let mut by_encoding: BTreeMap<&str, [usize; 4]> = BTreeMap::new();
    for op in &opcodes {
        let row = by_encoding.entry(&op.encoding).or_default();
        row[0] += 1;
        row[1 + op.cover as usize] += 1;
    }
    let list = |cover: Cover| -> Vec<&str> {
        opcodes
            .iter()
            .filter(|o| o.cover == cover)
            .map(|o| o.name.as_str())
            .collect()
    };
    let (named, neither) = (list(Cover::Named), list(Cover::Neither));

    if as_json {
        let rows = json::array(by_encoding.iter().map(|(e, [d, t, n, x])| {
            json::object([
                ("encoding", json::string(e)),
                ("decoded", d.to_string()),
                ("tested", t.to_string()),
                ("named_untested", n.to_string()),
                ("neither", x.to_string()),
            ])
        }));
        let names = |v: &[&str]| json::array(v.iter().map(|n| json::string(n)));
        println!(
            "{}",
            json::object([
                ("encodings", rows),
                ("named_untested", names(&named)),
                ("neither", names(&neither)),
            ])
        );
        return Ok(());
    }

    println!(
        "{:<9} {:>7} {:>7} {:>15} {:>8}",
        "encoding", "decoded", "tested", "named,untested", "neither"
    );
    let mut total = [0; 4];
    for (encoding, row) in &by_encoding {
        println!(
            "{encoding:<9} {:>7} {:>7} {:>15} {:>8}",
            row[0], row[1], row[2], row[3]
        );
        for (t, r) in total.iter_mut().zip(row) {
            *t += r;
        }
    }
    println!(
        "{:<9} {:>7} {:>7} {:>15} {:>8}",
        "all", total[0], total[1], total[2], total[3]
    );
    for (title, names) in [
        ("named in the lowering, never lowered by a test", &named),
        ("not named in the lowering (refused or generic)", &neither),
    ] {
        println!("\n{title}: {}", names.len());
        let shown = if all { names.len() } else { SHOWN };
        for chunk in names[..names.len().min(shown)].chunks(4) {
            println!("  {}", chunk.join("  "));
        }
        if names.len() > shown {
            println!("  ... {} more (--all)", names.len() - shown);
        }
    }
    Ok(())
}

/// (encoding, name) for every entry of every `<ENCODING>_ENTRIES` table.
fn parse_tables(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut encoding = None;
    for line in text.lines().map(str::trim) {
        // `const SOP2_ENTRIES: ...` or `pub(super) const SOP2: &[(u16, &str)] = &[`.
        let decl = line.strip_prefix("pub(super) ").unwrap_or(line);
        if let Some(rest) = decl.strip_prefix("const ")
            && let Some(name) = rest
                .split_once("_ENTRIES:")
                .or_else(|| rest.split_once(": &[(u16, &str)]"))
                .map(|(name, _)| name)
        {
            encoding = Some(name.to_owned());
            continue;
        }
        if line.starts_with("];") {
            encoding = None;
            continue;
        }
        if let Some(enc) = &encoding
            && let Some(name) = line.split('"').nth(1)
            && line.starts_with("(0x")
        {
            out.push((enc.clone(), name.to_owned()));
        }
    }
    out
}

/// The opcode names victory-shader's non-test source spells out, and the
/// prefixes it matches names by (`starts_with`, `strip_prefix`). The
/// decoder (gfx10/) is left out: it names every opcode.
fn names_in_lowering(src: &Path) -> Result<(BTreeSet<String>, Vec<String>)> {
    let mut literals = BTreeSet::new();
    let mut prefixes = BTreeSet::new();
    for entry in walkdir::WalkDir::new(src) {
        let entry = entry?;
        let path = entry.path();
        let file = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if path.extension().is_none_or(|e| e != "rs")
            || file.ends_with("tests.rs")
            || file == "coverage.rs"
            || path.strip_prefix(src).is_ok_and(|p| p.starts_with("gfx10"))
        {
            continue;
        }
        let text = std::fs::read_to_string(path)?;
        let code = without_test_module(&text);
        let (l, p) = names_in(code);
        literals.extend(l);
        prefixes.extend(p);
    }
    Ok((literals, prefixes.into_iter().collect()))
}

/// The text before a trailing `#[cfg(test)] mod ...`.
fn without_test_module(text: &str) -> &str {
    let mut at = 0;
    let mut lines = text.split_inclusive('\n').peekable();
    while let Some(line) = lines.next() {
        if line.trim() == "#[cfg(test)]"
            && lines
                .peek()
                .is_some_and(|next| next.trim_start().starts_with("mod "))
        {
            return &text[..at];
        }
        at += line.len();
    }
    text
}

fn names_in(code: &str) -> (Vec<String>, Vec<String>) {
    let looks_like_opcode = |s: &str| {
        s.len() > 2
            && s.contains('_')
            && s.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    };
    let mut literals = Vec::new();
    let mut prefixes = Vec::new();
    for (i, piece) in code.split('"').enumerate() {
        if i % 2 == 1 && looks_like_opcode(piece) {
            literals.push(piece.to_owned());
        }
    }
    for call in ["starts_with(\"", "strip_prefix(\""] {
        for rest in code.split(call).skip(1) {
            if let Some((p, _)) = rest.split_once('"')
                && looks_like_opcode(p)
                && p.len() >= 4
            {
                prefixes.push(p.to_owned());
            }
        }
    }
    (literals, prefixes)
}

fn read_list(file: &Path) -> Result<BTreeSet<String>> {
    let text =
        std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect())
}

/// Runs victory-shader's unit tests with the lowering recording what it
/// handled, and returns that.
fn run_tests(root: &Path) -> Result<BTreeSet<String>> {
    if let Err(e) = crate::agility::install(root, None, false) {
        eprintln!("opcodes: Agility SDK not installed, GPU-backed tests will fail: {e:#}");
    }
    let list = std::env::temp_dir().join("victory-opcode-coverage.txt");
    let _ = std::fs::remove_file(&list);
    eprintln!("opcodes: running victory-shader's unit tests (about 5 minutes)...");
    let out = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .args(["test", "-p", "victory-shader", "--lib", "--locked", "-q"])
        .env("VICTORY_OPCODE_COVERAGE", &list)
        .current_dir(root)
        .output()
        .context("starting cargo test")?;
    if !out.status.success() {
        eprintln!("opcodes: some tests failed; their opcodes may be missing");
    }
    let tested = read_list(&list).with_context(|| {
        format!(
            "the tests recorded nothing; cargo said:\n{}",
            String::from_utf8_lossy(&out.stderr)
        )
    })?;
    eprintln!(
        "opcodes: {} opcodes lowered in tests; list kept at {} (reuse with --from)",
        tested.len(),
        list.display()
    );
    Ok(tested)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_are_parsed_by_encoding() {
        let text = "\
const SOP2_ENTRIES: &[(u16, &str)] = &[
    (0x000, \"s_add_u32\"),
    (0x001, \"s_sub_u32\"),
];
const DS_ENTRIES: &[(u16, &str)] = &[
    (0x00d, \"ds_write_b32\"),
];
";
        let got = parse_tables(text);
        assert_eq!(
            got,
            [
                ("SOP2", "s_add_u32"),
                ("SOP2", "s_sub_u32"),
                ("DS", "ds_write_b32")
            ]
            .map(|(e, n)| (e.to_owned(), n.to_owned()))
        );
    }

    #[test]
    fn names_and_prefixes_come_from_code_not_tests() {
        let code = "\
fn lower(n: &str) {
    match n { \"v_add_f32\" => {}, x if x.starts_with(\"ds_read\") => {}, _ => {} }
    let msg = \"a message\";
}
#[cfg(test)]
mod tests { const T: &str = \"v_sub_f32\"; }
";
        let (l, p) = names_in(without_test_module(code));
        assert_eq!(l, ["v_add_f32", "ds_read"]);
        assert_eq!(p, ["ds_read"]);
    }

    #[test]
    fn the_real_tables_parse() {
        let text = std::fs::read_to_string(
            crate::workspace_root().join("crates/victory-shader/src/gfx10/opcodes.rs"),
        )
        .unwrap();
        let ops = parse_tables(&text);
        assert!(ops.len() > 1000, "{}", ops.len());
        assert!(ops.iter().any(|(e, n)| e == "VOP2" && n == "v_add_f32"));
    }
}
