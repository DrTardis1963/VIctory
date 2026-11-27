//! Tests of `.claude/hooks/asset_guard.py`, the asset policy at write time,
//! with synthetic tool calls. Skipped where Python is missing.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn python() -> Option<&'static str> {
    ["python", "python3", "py"].into_iter().find(|p| {
        Command::new(p)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    })
}

fn root() -> PathBuf {
    crate::workspace_root()
}

/// Whether the hook lets the tool call through (`true`) or refuses it.
fn allows(python: &str, tool: &str, input: &str) -> bool {
    let call = format!(
        "{{\"tool_name\":{},\"tool_input\":{input},\"cwd\":{}}}",
        crate::json::string(tool),
        crate::json::string(&root().display().to_string())
    );
    let mut child = Command::new(python)
        .args(["-I", "-S"])
        .arg(root().join(".claude/hooks/asset_guard.py"))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(call.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    match out.status.code() {
        Some(0) => true,
        Some(2) => false,
        other => panic!(
            "hook failed ({other:?}): {}",
            String::from_utf8_lossy(&out.stderr)
        ),
    }
}

fn write(path: &Path) -> String {
    format!(
        "{{\"file_path\":{},\"content\":\"x\"}}",
        crate::json::string(&path.display().to_string())
    )
}

fn command(text: &str) -> String {
    format!("{{\"command\":{}}}", crate::json::string(text))
}

#[test]
fn the_hook_refuses_data_written_into_the_checkout() {
    let Some(py) = python() else { return };
    let r = root();
    for path in ["dump.bin", "crates/notes.log", "target/frame.rdc"] {
        assert!(!allows(py, "Write", &write(&r.join(path))), "Write {path}");
    }
    for cmd in [
        "cp F:/game/title.exe ./",
        "cp F:/game/title.exe target/debug/",
        "cd crates && cp /f/game/world.pak .",
        "curl -sL -o capture.rdc https://example.invalid/a",
        "cargo test > test.log",
        "renderdoccmd capture --out-dir target/caps/ x | tee frames.png",
    ] {
        assert!(!allows(py, "Bash", &command(cmd)), "{cmd}");
    }
    let dest = r.join("crates").display().to_string() + "\\";
    let ps = format!("Copy-Item -Path F:\\game\\sky.dds -Destination '{dest}'");
    assert!(!allows(py, "PowerShell", &command(&ps)), "{ps}");
}

#[test]
fn the_hook_lets_ordinary_work_through() {
    let Some(py) = python() else { return };
    let r = root();
    assert!(allows(py, "Write", &write(&r.join("crates/x/src/lib.rs"))));
    let outside = std::env::temp_dir().join("capture.rdc");
    assert!(allows(py, "Write", &write(&outside)));
    for cmd in [
        "cargo test 2>&1 | grep -o \"test result\" | head -3",
        "cat > ../msg.txt <<'EOF'\nsubject\n\nCo-Authored-By: A <a@b.invalid> > x.bin\nEOF\ngit commit -F ../msg.txt",
        "cp F:/VIctory_Testing/D3D12/D3D12Core.dll target/debug/deps/D3D12/",
        "cp target/debug/victory.exe target/diag/",
        "cp target/debug/victory.exe F:/VIctory_Testing/",
        "cargo build > target/build.log 2> /dev/null",
    ] {
        assert!(allows(py, "Bash", &command(cmd)), "{cmd}");
    }
    let patch = std::env::temp_dir().join("change.patch");
    let cmd = format!("git diff > '{}'", patch.display());
    assert!(allows(py, "Bash", &command(&cmd)), "{cmd}");
    let ps = "cargo test *> $null; Copy-Item README.md -Destination F:\\notes\\";
    assert!(allows(py, "PowerShell", &command(ps)), "{ps}");
}

/// The hook's list of allowed extensions is the policy's.
#[test]
fn the_hook_allows_what_the_policy_allows() {
    let hook = std::fs::read_to_string(root().join(".claude/hooks/asset_guard.py")).unwrap();
    let list = hook
        .split("ALLOWED_EXTENSIONS = {")
        .nth(1)
        .and_then(|s| s.split('}').next())
        .unwrap();
    let mut hook_exts: Vec<&str> = list
        .split(',')
        .map(|e| e.trim().trim_matches('"'))
        .filter(|e| !e.is_empty())
        .collect();
    hook_exts.sort_unstable();
    let mut policy = crate::policy::ALLOWED_EXTENSIONS.to_vec();
    policy.sort_unstable();
    assert_eq!(hook_exts, policy);
}
