//! Test builds only: which opcodes the lowering handled, for
//! `cargo xtask opcodes`. With `VICTORY_OPCODE_COVERAGE` naming a file
//! (outside the repository), each opcode lowered without a refusal is
//! appended to it, once per test process.

use std::collections::HashSet;
use std::sync::Mutex;

static SEEN: Mutex<Option<HashSet<&'static str>>> = Mutex::new(None);

pub(super) fn record(name: &'static str) {
    let Some(file) = std::env::var_os("VICTORY_OPCODE_COVERAGE") else {
        return;
    };
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    if seen.get_or_insert_with(HashSet::new).insert(name) {
        use std::io::Write;
        let opened = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(file);
        if let Ok(mut f) = opened {
            let _ = writeln!(f, "{name}");
        }
    }
}
