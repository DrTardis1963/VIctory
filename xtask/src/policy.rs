//! Rule #1, statically: the repository only ever contains small text files
//! that we wrote. See docs/asset-policy.md.
//!
//! This is an allowlist, not a blocklist: a file passes only if its name is
//! on the list, it is not too big, it is text, and it does not smuggle data
//! inside text (long hex/base64 runs).

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

const MAX_FILE_SIZE: usize = 512 * 1024;
const MAX_ENCODED_RUN: usize = 512;
const MAX_ESCAPED_HEX_RUN: usize = 128;
const MAX_HEX_ARRAY_RUN: usize = 256;

const ALLOWED_EXTENSIONS: &[&str] = &[
    "rs", "toml", "md", "txt", "lock", "yml", "yaml", "json", "hlsl", "hlsli", "ps1", "cmd", "sh",
];
const ALLOWED_NAMES: &[&str] = &[
    ".gitignore",
    ".gitattributes",
    ".editorconfig",
    ".victory-root",
    "pre-commit",
    "LICENSE",
    "LICENSE-MIT",
    "LICENSE-APACHE",
];

/// Paths explicitly exempted, with pinned BLAKE3 hash and reason.
/// Keep this list empty if at all possible.
const EXCEPTIONS: &[(&str, &str, &str)] = &[(
    "crates/victory-formats/src/bcpack-tables.bin",
    "f0f032aa310e7cfd431c075d2816decdd39d21b1927cbffc38386599c0de73c0",
    "the BCPack format's constant tables: nothing decodes without them and they cannot be derived (owner's decision, 2026-11-23; docs/asset-policy.md)",
)];

/// Signatures of formats that must never be committed.
const FORBIDDEN_MAGIC: &[(&[u8], &str)] = &[
    (b"MZ", "PE executable"),
    (b"DXBC", "shader container"),
    (b"DDS ", "DDS texture"),
    (b"RIFF", "RIFF (audio/video)"),
    (b"BC\xC0\xDE", "LLVM bitcode"),
    (b"Microsoft C/C++ MSF", "PDB"),
    (b"!<arch>", "library archive"),
    (b"PK\x03\x04", "zip"),
    (b"7z\xBC\xAF", "7-zip"),
    (b"MSCF", "cabinet"),
    (b"\xD0\xCF\x11\xE0", "MSI/OLE"),
];

pub fn run(root: &Path, staged: bool) -> Result<()> {
    let files = list_files(root, staged)?;
    let mut violations = Vec::new();
    for path in &files {
        let content = if staged {
            let out = Command::new("git")
                .args(["show", &format!(":{path}")])
                .current_dir(root)
                .output()?;
            if !out.status.success() {
                continue; // deleted in the index
            }
            out.stdout
        } else {
            match std::fs::read(root.join(path)) {
                Ok(c) => c,
                Err(_) => continue,
            }
        };
        if let Some((_, expected_hash, _)) = EXCEPTIONS.iter().find(|(p, _, _)| p == path) {
            if let Some(problem) = check_exception(expected_hash, &content) {
                violations.push(format!("{path}: {problem}"));
            }
            continue;
        }
        for problem in check(path, &content) {
            violations.push(format!("{path}: {problem}"));
        }
    }
    if violations.is_empty() {
        println!("asset policy: {} files ok", files.len());
        return Ok(());
    }
    for v in &violations {
        eprintln!("POLICY: {v}");
    }
    bail!(
        "{} asset-policy violation(s). Nothing from the game may be committed; \
         see docs/asset-policy.md",
        violations.len()
    )
}

fn list_files(root: &Path, staged: bool) -> Result<Vec<String>> {
    let git = |args: &[&str]| -> Result<Option<Vec<String>>> {
        let out = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .context("running git")?;
        if !out.status.success() {
            return Ok(None);
        }
        Ok(Some(
            out.stdout
                .split(|&b| b == 0)
                .filter(|s| !s.is_empty())
                .map(|s| String::from_utf8_lossy(s).replace('\\', "/"))
                .collect(),
        ))
    };
    if staged {
        return git(&[
            "diff",
            "--cached",
            "--name-only",
            "--diff-filter=ACMR",
            "-z",
        ])?
        .context("--staged needs a git repository");
    }
    if let Some(mut tracked) = git(&[
        "ls-files",
        "-z",
        "--cached",
        "--others",
        "--exclude-standard",
    ])? {
        tracked.sort();
        tracked.dedup();
        return Ok(tracked);
    }
    // Not a git repository: walk everything except build output.
    Ok(walkdir::WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| !matches!(e.file_name().to_str(), Some("target" | ".git")))
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| {
            e.path()
                .strip_prefix(root)
                .ok()
                .map(|p| p.to_string_lossy().replace('\\', "/"))
        })
        .collect())
}

/// All problems with one file. Pure, for testing.
pub fn check(path: &str, content: &[u8]) -> Vec<String> {
    let mut problems = Vec::new();
    let name = path.rsplit('/').next().unwrap_or(path);
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    let allowed = ALLOWED_NAMES.contains(&name)
        || ext
            .as_deref()
            .is_some_and(|e| ALLOWED_EXTENSIONS.contains(&e));
    if !allowed {
        problems.push("file type not on the allowlist".to_owned());
    }
    if content.len() > MAX_FILE_SIZE {
        problems.push(format!(
            "{} bytes exceeds the {MAX_FILE_SIZE}-byte limit",
            content.len()
        ));
    }
    for (magic, what) in FORBIDDEN_MAGIC {
        if content.starts_with(magic) {
            problems.push(format!("looks like a {what}"));
        }
    }
    if content.contains(&0) {
        problems.push("binary content (NUL bytes)".to_owned());
    } else {
        if let Some(run) = longest_encoded_run(content)
            && run > MAX_ENCODED_RUN
        {
            problems.push(format!("{run}-character hex/base64 run (embedded data?)"));
        }
        let escapes = longest_escaped_hex_run(content);
        if escapes > MAX_ESCAPED_HEX_RUN {
            problems.push(format!("{escapes}-byte \\x escape run (embedded data?)"));
        }
        let hex_array = longest_hex_array_run(content);
        if hex_array > MAX_HEX_ARRAY_RUN {
            problems.push(format!("{hex_array}-byte hex array run (embedded data?)"));
        }
        if let Some(kind) = detect_credential(content) {
            problems.push(format!("looks like a committed {kind}"));
        }
    }
    problems
}

/// Verifies an exempted file against its pinned BLAKE3 hex hash.
pub fn check_exception(expected_blake3: &str, content: &[u8]) -> Option<String> {
    let actual = blake3::hash(content).to_hex();
    if actual.as_str() == expected_blake3 {
        None
    } else {
        Some(format!(
            "exempted file BLAKE3 changed (got {actual}, expected {expected_blake3})"
        ))
    }
}

/// Longest run of characters that could be hex or base64 payload.
fn longest_encoded_run(content: &[u8]) -> Option<usize> {
    let is_payload = |b: &u8| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=');
    content.split(|b| !is_payload(b)).map(<[u8]>::len).max()
}

/// Longest contiguous run of `\xHH` escapes (allowing quotes/whitespace between literal lines).
fn longest_escaped_hex_run(content: &[u8]) -> usize {
    let (mut best, mut cur, mut i) = (0, 0, 0);
    while i < content.len() {
        if i + 3 < content.len()
            && content[i] == b'\\'
            && content[i + 1] == b'x'
            && content[i + 2].is_ascii_hexdigit()
            && content[i + 3].is_ascii_hexdigit()
        {
            cur += 1;
            best = best.max(cur);
            i += 4;
        } else if cur > 0 && (content[i].is_ascii_whitespace() || content[i] == b'"') {
            i += 1;
        } else {
            cur = 0;
            i += 1;
        }
    }
    best
}

/// Longest comma/whitespace-separated sequence of `0xH` or `0xHH` hex byte literals.
fn longest_hex_array_run(content: &[u8]) -> usize {
    let (mut best, mut cur, mut i) = (0, 0, 0);
    while i < content.len() {
        if i + 2 < content.len()
            && content[i] == b'0'
            && (content[i + 1] == b'x' || content[i + 1] == b'X')
            && content[i + 2].is_ascii_hexdigit()
        {
            let start = i + 2;
            let mut end = start;
            while end < content.len() && content[end].is_ascii_hexdigit() {
                end += 1;
            }
            if (1..=2).contains(&(end - start))
                && (i == 0 || !content[i - 1].is_ascii_alphanumeric())
            {
                cur += 1;
                best = best.max(cur);
                i = end;
                continue;
            }
            cur = 0;
            i = end;
        } else if cur > 0 && (content[i].is_ascii_whitespace() || content[i] == b',') {
            i += 1;
        } else {
            cur = 0;
            i += 1;
        }
    }
    best
}

/// Detects private-key PEM headers or GitHub token prefixes in text files.
fn detect_credential(content: &[u8]) -> Option<&'static str> {
    let Ok(text) = std::str::from_utf8(content) else {
        return None;
    };
    let begin = concat!("-----BEGIN ", "");
    let key_end = concat!("PRIVATE ", "KEY-----");
    let mut rest = text;
    while let Some(idx) = rest.find(begin) {
        let after = &rest[idx + begin.len()..];
        let window = &after[..after.len().min(40)];
        if window.contains(key_end) {
            return Some("private key");
        }
        rest = after;
    }
    let prefixes = [
        concat!("gh", "p_"),
        concat!("gh", "o_"),
        concat!("gh", "u_"),
        concat!("gh", "s_"),
        concat!("gh", "r_"),
        concat!("github", "_pat_"),
    ];
    for prefix in prefixes {
        let mut scan = text;
        while let Some(idx) = scan.find(prefix) {
            let boundary = idx == 0 || !scan.as_bytes()[idx - 1].is_ascii_alphanumeric();
            let tail = &scan[idx + prefix.len()..];
            let token_len = tail
                .bytes()
                .take_while(|b| b.is_ascii_alphanumeric() || *b == b'_')
                .count();
            if boundary && token_len >= 20 {
                return Some("GitHub token");
            }
            scan = tail;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::check;

    /// Enforces the policy wherever tests run, even in clones without the hook.
    #[test]
    fn repository_passes_policy() {
        super::run(&crate::workspace_root(), false).unwrap();
    }

    #[test]
    fn accepts_source() {
        assert!(check("crates/x/src/lib.rs", b"fn main() {}\n").is_empty());
        assert!(check(".gitignore", b"/target/\n").is_empty());
        assert!(check(".githooks/pre-commit", b"#!/bin/sh\n").is_empty());
    }

    #[test]
    fn rejects_game_like_files() {
        assert!(!check("assets/tex.dds", b"DDS \x7c\0\0\0").is_empty());
        assert!(
            !check("notes.txt", b"MZ\x90\0").is_empty(),
            "magic wins over extension"
        );
        assert!(
            !check("data.bin", b"hello").is_empty(),
            "extension not allowed"
        );
        assert!(
            !check("a.rs", &vec![b'a'; 600 * 1024]).is_empty(),
            "too big"
        );
    }

    #[test]
    fn rejects_data_smuggled_into_text() {
        let blob = format!("const DATA: &str = \"{}\";", "A1".repeat(400));
        assert!(!check("src/blob.rs", blob.as_bytes()).is_empty());

        let escaped = format!("const DATA: &[u8] = b\"{}\";", "\\x41".repeat(150));
        assert!(!check("src/esc.rs", escaped.as_bytes()).is_empty());

        let hex_array = format!(
            "const DATA: &[u8] = &[{}];",
            std::iter::repeat_n("0x41", 260).collect::<Vec<_>>().join(", ")
        );
        assert!(!check("src/arr.rs", hex_array.as_bytes()).is_empty());
    }

    #[test]
    fn rejects_tampered_exception_and_credentials() {
        let expected = super::EXCEPTIONS[0].1;
        assert!(super::check_exception(expected, b"tampered").is_some());

        let token = format!("{}0123456789abcdefghij", concat!("gh", "p_"));
        assert!(!check("src/secret.rs", token.as_bytes()).is_empty());

        let pem = format!("-----BEGIN RSA {}-----", concat!("PRIVATE ", "KEY"));
        assert!(!check("src/key.txt", pem.as_bytes()).is_empty());
    }
}
