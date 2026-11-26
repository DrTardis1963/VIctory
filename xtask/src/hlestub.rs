//! `cargo xtask hle-stub <dll> <fn>`: scaffolds a new HLE function.
//!
//! It finds the module file serving `<dll>` (as `cargo xtask todo` does,
//! from each module's `dlls: &[...]`) and adds three things in its style:
//!
//! - a handler named like the import, which does nothing, says so once,
//!   and returns a default (0: `S_OK`, `FALSE`, null; `--returns` sets
//!   another), with the four register arguments as raw words until it gets
//!   the real signature;
//! - its `hle_export!` in the module's `exports: vec![...]`;
//! - a unit test calling it.
//!
//! Without `--write` it prints the patch and changes nothing.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

pub struct Options {
    pub dll: String,
    pub function: String,
    pub write: bool,
    /// The module file to add to, when several serve the DLL.
    pub module: Option<PathBuf>,
    /// The modules folder (default: victory-hle's); tests point it at a copy.
    pub modules_dir: Option<PathBuf>,
    pub returns: String,
}

pub fn run(root: &Path, options: &Options) -> Result<()> {
    let modules = options
        .modules_dir
        .clone()
        .unwrap_or_else(|| root.join("crates/victory-hle/src/modules"));
    let file = match &options.module {
        Some(file) => file.clone(),
        None => module_for(&modules, &options.dll)?,
    };
    let source =
        std::fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
    let insertions = plan(&source, &options.dll, &options.function, &options.returns)?;
    let shown = file
        .strip_prefix(root)
        .unwrap_or(&file)
        .display()
        .to_string();
    if options.write {
        std::fs::write(&file, apply(&source, &insertions))
            .with_context(|| format!("writing {}", file.display()))?;
        println!(
            "added {}!{} to {shown}: a handler, its export, a test",
            options.dll, options.function
        );
    } else {
        print!("{}", patch(&shown, &source, &insertions));
        println!("(nothing changed; --write applies it)");
    }
    Ok(())
}

/// The module file serving `dll`: the one whose list names it exactly, else
/// the first whose pattern matches.
fn module_for(modules: &Path, dll: &str) -> Result<PathBuf> {
    let served = crate::todo::served_dlls(modules)?;
    let files = crate::todo::modules_serving(dll, &served);
    let exact = served
        .iter()
        .find(|(pattern, _)| pattern.eq_ignore_ascii_case(dll))
        .map(|(_, file)| file.clone());
    let Some(chosen) = exact.or_else(|| files.first().cloned()) else {
        bail!(
            "no module in {} serves {dll}: add a module for it to modules/mod.rs first",
            modules.display()
        );
    };
    if files.len() > 1 {
        eprintln!(
            "{dll} is served by {}; adding to {chosen} (--module picks another)",
            files.join(", ")
        );
    }
    // `served_dlls` names files `modules/<path>`.
    let relative = chosen.strip_prefix("modules/").unwrap_or(&chosen);
    Ok(modules.join(relative))
}

/// Lines to insert before line `at` (0-based) of the original source.
#[derive(Debug, PartialEq, Eq)]
struct Insertion {
    at: usize,
    lines: Vec<String>,
}

fn plan(source: &str, dll: &str, function: &str, returns: &str) -> Result<Vec<Insertion>> {
    let ident = function
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_')
        && function.chars().next().is_some_and(|c| !c.is_ascii_digit());
    if !ident {
        bail!(
            "{function:?} is not a Rust identifier (an ordinal import needs a hand-written HleExport)"
        );
    }
    if source.contains(&format!("hle_export!({function})"))
        || source.contains(&format!("name: \"{function}\""))
        || source.contains(&format!("fn {function}("))
    {
        bail!("{function} is already in this module");
    }
    let lines: Vec<&str> = source.lines().collect();
    let line_of = |offset: usize| source[..offset].matches('\n').count();

    // The handler goes before `fn module()`, with its doc comment.
    let module_fn = source
        .find("fn module() -> HleModule")
        .context("the file has no `fn module() -> HleModule`")?;
    let module_line = line_of(module_fn);
    let mut handler_at = module_line;
    while handler_at > 0 && lines[handler_at - 1].trim_start().starts_with(['#', '/']) {
        handler_at -= 1;
    }

    // The export goes last in `exports: vec![...]`.
    let open = source[module_fn..]
        .find("exports: vec![")
        .map(|i| module_fn + i + "exports: vec!".len())
        .context("`fn module()` lists no `exports: vec![...]`")?;
    let close = matching(source, open).context("the exports list does not close")?;
    let close_line = line_of(close);
    let (export_at, indent) = if line_of(open) == close_line {
        bail!("the exports list is on one line; put its entries on lines of their own first");
    } else {
        let last = lines[..close_line]
            .iter()
            .rev()
            .find(|l| !l.trim().is_empty())
            .copied()
            .unwrap_or("");
        let indent: String = last.chars().take_while(|c| c.is_whitespace()).collect();
        (close_line, indent)
    };
    let export_macro = if source.contains("hle_export") && has_macro_import(source) {
        "hle_export!"
    } else {
        "crate::hle_export!"
    };

    let handler = vec![
        format!("/// `{dll}!{function}`, scaffolded by `cargo xtask hle-stub`: not implemented"),
        format!("/// yet. It does nothing and returns {returns}; it says so the first time."),
        "// shortcut: a stub; give it the real signature and behaviour.".to_owned(),
        "#[allow(non_snake_case)]".to_owned(),
        format!("extern \"system\" fn {function}(a: usize, b: usize, c: usize, d: usize) -> u64 {{"),
        "    static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);"
            .to_owned(),
        "    if !SAID.swap(true, std::sync::atomic::Ordering::Relaxed) {".to_owned(),
        format!("        tracing::warn!(a, b, c, d, \"{dll}!{function}: a stub, returning {returns}\");"),
        "    }".to_owned(),
        format!("    {returns}"),
        "}".to_owned(),
        String::new(),
    ];
    let mut insertions = vec![
        Insertion {
            at: handler_at,
            lines: handler,
        },
        Insertion {
            at: export_at,
            lines: vec![format!("{indent}{export_macro}({function}),")],
        },
    ];

    let test = |indent: &str| {
        vec![
            format!("{indent}#[test]"),
            format!(
                "{indent}fn {}_is_a_stub_returning_its_default() {{",
                snake(function)
            ),
            format!("{indent}    assert_eq!({function}(0, 0, 0, 0), {returns});"),
            format!("{indent}}}"),
        ]
    };
    match source.find("mod tests {") {
        Some(start) => {
            let open = start + "mod tests ".len();
            let close = matching(source, open).context("`mod tests` does not close")?;
            let mut lines = vec![String::new()];
            lines.extend(test("    "));
            insertions.push(Insertion {
                at: line_of(close),
                lines,
            });
        }
        None => {
            let mut lines = vec![
                String::new(),
                "#[cfg(test)]".to_owned(),
                "mod tests {".to_owned(),
                "    use super::*;".to_owned(),
                String::new(),
            ];
            lines.extend(test("    "));
            lines.push("}".to_owned());
            insertions.push(Insertion {
                at: lines_count(source),
                lines,
            });
        }
    }
    insertions.sort_by_key(|i| i.at);
    Ok(insertions)
}

fn lines_count(source: &str) -> usize {
    source.lines().count()
}

/// Whether `hle_export` is brought in by a `use` line.
fn has_macro_import(source: &str) -> bool {
    source
        .lines()
        .any(|l| l.trim_start().starts_with("use ") && l.contains("hle_export"))
}

/// `XGameFooBar` as `x_game_foo_bar`.
fn snake(name: &str) -> String {
    let mut out = String::new();
    let chars: Vec<char> = name.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if c.is_ascii_uppercase() {
            let after_lower =
                i > 0 && (chars[i - 1].is_ascii_lowercase() || chars[i - 1].is_ascii_digit());
            let before_lower = chars.get(i + 1).is_some_and(|n| n.is_ascii_lowercase());
            let acronym_end = i > 0 && chars[i - 1].is_ascii_uppercase() && before_lower;
            if !out.is_empty() && !out.ends_with('_') && (after_lower || acronym_end) {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// The offset of the bracket closing the one at `open`, skipping strings,
/// characters and comments.
fn matching(source: &str, open: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let (opener, closer) = match bytes.get(open)? {
        b'[' => (b'[', b']'),
        b'{' => (b'{', b'}'),
        b'(' => (b'(', b')'),
        _ => return None,
    };
    let mut depth = 0usize;
    let mut i = open;
    while i < bytes.len() {
        match bytes[i] {
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i += 1;
            }
            b'"' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    if bytes[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            b'r' if bytes.get(i + 1).is_some_and(|&b| b == b'#' || b == b'"')
                && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric() && bytes[i - 1] != b'_') =>
            {
                let hashes = bytes[i + 1..].iter().take_while(|&&b| b == b'#').count();
                if bytes.get(i + 1 + hashes) == Some(&b'"') {
                    let end = format!("\"{}", "#".repeat(hashes));
                    let from = i + 2 + hashes;
                    i = source[from..]
                        .find(&end)
                        .map_or(bytes.len(), |e| from + e + end.len() - 1);
                }
            }
            b'\'' => {
                // 'x' or '\n' is a character; 'a without a closing quote a lifetime.
                if bytes.get(i + 1) == Some(&b'\\') {
                    i += 2;
                    while i < bytes.len() && bytes[i] != b'\'' {
                        i += 1;
                    }
                } else if let Some(c) = source[i + 1..].chars().next()
                    && source[i + 1 + c.len_utf8()..].starts_with('\'')
                {
                    i += 1 + c.len_utf8();
                }
            }
            b if b == opener => depth += 1,
            b if b == closer => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn apply(source: &str, insertions: &[Insertion]) -> String {
    let mut out = String::with_capacity(source.len() + 1024);
    let lines: Vec<&str> = source.lines().collect();
    let mut next = insertions.iter().peekable();
    for (i, line) in lines.iter().enumerate() {
        while let Some(ins) = next.next_if(|ins| ins.at == i) {
            for l in &ins.lines {
                out.push_str(l);
                out.push('\n');
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    for ins in next {
        for l in &ins.lines {
            out.push_str(l);
            out.push('\n');
        }
    }
    out
}

/// The insertions as a unified diff, three lines of context each.
fn patch(file: &str, source: &str, insertions: &[Insertion]) -> String {
    let lines: Vec<&str> = source.lines().collect();
    let mut out = format!("--- a/{file}\n+++ b/{file}\n");
    let mut added = 0;
    for ins in insertions {
        let from = ins.at.saturating_sub(3);
        let to = (ins.at + 3).min(lines.len());
        let context = to - from;
        out += &format!(
            "@@ -{},{context} +{},{} @@\n",
            from + 1,
            from + 1 + added,
            context + ins.lines.len()
        );
        for l in &lines[from..ins.at] {
            out += &format!(" {l}\n");
        }
        for l in &ins.lines {
            out += &format!("+{l}\n");
        }
        for l in &lines[ins.at..to] {
            out += &format!(" {l}\n");
        }
        added += ins.lines.len();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A module file as the HLE crate writes them (synthetic: no real
    /// module is touched).
    const MODULE: &str = r#"//! A synthetic module.
#![allow(non_snake_case)]

use crate::{HleModule, hle_export};

/// Says "{hi}" ['x'] and has a lifetime 'a.
extern "system" fn XProbeOpen(_: usize) -> i32 {
    let _brace = '{';
    0
}

/// The module.
pub(crate) fn module() -> HleModule {
    HleModule {
        name: "probe",
        dlls: &["xprobe.dll", "api-ms-win-probe-*"],
        exports: vec![
            hle_export!(XProbeOpen),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_works() {
        assert_eq!(XProbeOpen(0), 0);
    }
}
"#;

    fn options(dir: &Path, write: bool) -> Options {
        Options {
            dll: "XPROBE.dll".into(),
            function: "XProbeGetHTTPStatus".into(),
            write,
            module: None,
            modules_dir: Some(dir.to_owned()),
            returns: "0".into(),
        }
    }

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "victory-hle-stub-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("probe.rs"), MODULE).unwrap();
        dir
    }

    #[test]
    fn the_stub_goes_before_the_module_its_export_last_and_a_test_in_tests() {
        let insertions = plan(MODULE, "xprobe.dll", "XProbeClose", "0").unwrap();
        let text = apply(MODULE, &insertions);
        let handler = text.find("extern \"system\" fn XProbeClose(").unwrap();
        let doc = text
            .find("/// `xprobe.dll!XProbeClose`, scaffolded")
            .unwrap();
        let module = text.find("/// The module.").unwrap();
        assert!(doc < handler && handler < module, "{text}");
        assert!(
            text.contains("            hle_export!(XProbeOpen),\n            hle_export!(XProbeClose),\n        ],"),
            "{text}"
        );
        assert!(
            text.contains("    }\n\n    #[test]\n    fn x_probe_close_is_a_stub_returning_its_default() {\n        assert_eq!(XProbeClose(0, 0, 0, 0), 0);\n    }\n}\n"),
            "{text}"
        );
        assert!(text.contains(
            "tracing::warn!(a, b, c, d, \"xprobe.dll!XProbeClose: a stub, returning 0\");"
        ));
    }

    #[test]
    fn without_write_nothing_changes_and_with_it_the_file_is_edited() {
        let dir = scratch();
        let file = dir.join("probe.rs");
        let root = Path::new("Z:/nowhere");
        run(root, &options(&dir, false)).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), MODULE);
        run(root, &options(&dir, true)).unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("hle_export!(XProbeGetHTTPStatus),"), "{text}");
        assert!(text.contains("fn x_probe_get_http_status_is_a_stub_returning_its_default()"));
        // A second time it is already there.
        let again = run(root, &options(&dir, true)).unwrap_err().to_string();
        assert!(again.contains("already in this module"), "{again}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_patch_shows_each_insertion_with_context() {
        let insertions = plan(MODULE, "xprobe.dll", "XProbeClose", "0").unwrap();
        let text = patch("modules/probe.rs", MODULE, &insertions);
        assert!(
            text.starts_with("--- a/modules/probe.rs\n+++ b/modules/probe.rs\n@@ -"),
            "{text}"
        );
        assert_eq!(text.matches("\n@@ ").count(), 3, "{text}");
        assert!(
            text.contains("\n+            hle_export!(XProbeClose),\n         ],\n"),
            "{text}"
        );
    }

    #[test]
    fn a_module_without_tests_gets_a_tests_module() {
        let bare = MODULE.split("#[cfg(test)]").next().unwrap();
        let text = apply(bare, &plan(bare, "xprobe.dll", "XProbeClose", "7").unwrap());
        assert!(text.ends_with("mod tests {\n    use super::*;\n\n    #[test]\n    fn x_probe_close_is_a_stub_returning_its_default() {\n        assert_eq!(XProbeClose(0, 0, 0, 0), 7);\n    }\n}\n"), "{text}");
    }

    #[test]
    fn a_dll_no_module_serves_and_odd_names_are_refused() {
        let dir = scratch();
        let mut o = options(&dir, false);
        o.dll = "unknown.dll".into();
        let e = run(Path::new("Z:/"), &o).unwrap_err().to_string();
        assert!(e.contains("no module in"), "{e}");
        // Through the prefix pattern.
        assert_eq!(
            module_for(&dir, "api-ms-win-probe-l1-1-0.dll").unwrap(),
            dir.join("probe.rs")
        );
        assert!(plan(MODULE, "x.dll", "#12", "0").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn names_become_snake_case() {
        assert_eq!(snake("XGameFooBar"), "x_game_foo_bar");
        assert_eq!(snake("XProbeGetHTTPStatus"), "x_probe_get_http_status");
        assert_eq!(snake("D3D12CreateDevice"), "d3_d12_create_device");
        assert_eq!(snake("already_snake"), "already_snake");
    }

    #[test]
    fn brackets_match_past_strings_characters_and_comments() {
        let s = r#"[ "]" ']' /* ] */ // ]
 r"]" 'a ]"#;
        assert_eq!(matching(s, 0), Some(s.len() - 1));
    }
}
