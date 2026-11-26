//! Stubs for imports nobody implements yet.
//!
//! Each unresolved import gets 32 bytes of generated code:
//!
//! ```text
//! mov rdx, [rsp]                   ; 48 8B 14 24  (the title's return address)
//! mov rcx, <&'static ImportSite>   ; 48 B9 imm64
//! mov rax, <unimplemented_import>  ; 48 B8 imm64
//! jmp rax                          ; FF E0
//! ```
//!
//! The jump keeps the title's return address on the stack, so the handler
//! returns straight to the caller. Bring-up then shows exactly which API to
//! implement next, rather than failing at load time.
//!
//! Every site keeps its call count and first caller. Crash reports list the
//! ones called, and [`save_unimplemented_to`] keeps a ledger file of them
//! for `cargo xtask todo`, which ranks what to implement next.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock, TryLockError};

use anyhow::{Result, bail};
use windows_sys::Win32::System::Diagnostics::Debug::{
    DebugBreak, FlushInstructionCache, IsDebuggerPresent,
};
use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, MEM_RESERVE, PAGE_EXECUTE_READ, PAGE_READWRITE, VirtualAlloc, VirtualProtect,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

/// What an unimplemented import does when called.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UnimplementedPolicy {
    /// Log, break into an attached debugger, then abort. The default:
    /// silently continuing usually crashes later and further from the cause.
    #[default]
    Trap,
    /// Log once per import and return 0. Useful to push through bring-up.
    ReturnZero,
}

/// Identifies one unresolved import. Leaked: lives as long as the process.
#[derive(Debug)]
pub struct ImportSite {
    pub dll: String,
    pub symbol: String,
    pub policy: UnimplementedPolicy,
    pub calls: AtomicU64,
    /// The return address of the first call (0 before it): where in the
    /// title the import is needed.
    pub first_caller: AtomicUsize,
}

impl ImportSite {
    /// A site for `dll!symbol`, leaked and listed for the ledger.
    pub(crate) fn leak(dll: &str, symbol: &str, policy: UnimplementedPolicy) -> &'static Self {
        let site: &'static Self = Box::leak(Box::new(ImportSite {
            dll: dll.to_owned(),
            symbol: symbol.to_owned(),
            policy,
            calls: AtomicU64::new(0),
            first_caller: AtomicUsize::new(0),
        }));
        SITES.lock().unwrap_or_else(|e| e.into_inner()).push(site);
        site
    }
}

/// Every site made, for the ledger.
static SITES: Mutex<Vec<&'static ImportSite>> = Mutex::new(Vec::new());

static UNIMPLEMENTED_CALLS: AtomicU64 = AtomicU64::new(0);

/// Total number of calls that reached an unimplemented-import stub.
pub fn unimplemented_call_count() -> u64 {
    UNIMPLEMENTED_CALLS.load(Ordering::Relaxed)
}

extern "system" fn unimplemented_import(site: &'static ImportSite, caller: usize) -> u64 {
    UNIMPLEMENTED_CALLS.fetch_add(1, Ordering::Relaxed);
    let previous = site.calls.fetch_add(1, Ordering::Relaxed);
    let _ = site
        .first_caller
        .compare_exchange(0, caller, Ordering::Relaxed, Ordering::Relaxed);
    match site.policy {
        UnimplementedPolicy::ReturnZero => {
            if previous == 0 {
                let at = crate::crash::locate(caller);
                tracing::warn!(dll = %site.dll, symbol = %site.symbol, %at, "unimplemented import called, returning 0");
                // A new entry: the ledger is current whenever a title stops
                // in a way nothing reports (closed, killed).
                save_ledger();
            }
            0
        }
        UnimplementedPolicy::Trap => {
            let at = crate::crash::locate(caller);
            tracing::error!(dll = %site.dll, symbol = %site.symbol, %at, "unimplemented import called");
            // Into the crash log too (which also saves the ledger): `abort`
            // ends the process without running the handlers that write it.
            let (dll, symbol) = (&site.dll, &site.symbol);
            let text = format!("VIctory: unimplemented import {dll}!{symbol} called from {at}");
            crate::crash::log_crash_at(&text, Some(caller));
            // SAFETY: plain Win32 calls without preconditions.
            unsafe {
                if IsDebuggerPresent() != 0 {
                    DebugBreak();
                }
            }
            std::process::abort();
        }
    }
}

/// One unimplemented import the title called.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnimplementedCall {
    pub dll: String,
    pub symbol: String,
    pub calls: u64,
    /// `module+rva` of the first call's return address.
    pub first_caller: String,
}

/// The unimplemented imports called so far, most called first; `None` if
/// another thread holds the list (it may be the one that faulted).
pub fn unimplemented_calls() -> Option<Vec<UnimplementedCall>> {
    let sites = match SITES.try_lock() {
        Ok(sites) => sites,
        Err(TryLockError::Poisoned(sites)) => sites.into_inner(),
        Err(TryLockError::WouldBlock) => return None,
    };
    let mut called: Vec<_> = sites
        .iter()
        .filter_map(|site| {
            let calls = site.calls.load(Ordering::Relaxed);
            (calls > 0).then(|| UnimplementedCall {
                dll: site.dll.clone(),
                symbol: site.symbol.clone(),
                calls,
                first_caller: crate::crash::locate(site.first_caller.load(Ordering::Relaxed)),
            })
        })
        .collect();
    drop(sites);
    called.sort_by(|a, b| (b.calls, &a.dll, &a.symbol).cmp(&(a.calls, &b.dll, &b.symbol)));
    Some(called)
}

/// The ledger file, once [`save_unimplemented_to`] names it.
static LEDGER: OnceLock<PathBuf> = OnceLock::new();

/// Keeps `file` listing the unimplemented imports the title calls, for
/// `cargo xtask todo`. It is saved when an import is first called, with
/// every crash report or trap, at process exit, and on every call of this
/// (the first call's `file` stays the ledger); call counts in between are
/// as of the last save.
pub fn save_unimplemented_to(file: PathBuf) {
    let _ = LEDGER.set(file);
    save_ledger();
}

/// Saves the ledger file, if one is named. Whole or not at all: each save
/// is staged and renamed into place, so racing saves never mix.
pub(crate) fn save_ledger() {
    let Some(file) = LEDGER.get() else { return };
    let Some(called) = unimplemented_calls() else {
        return;
    };
    if let Err(error) = write_staged(file, ledger_toml(&called).as_bytes()) {
        tracing::warn!(file = %file.display(), %error, "saving the unimplemented-import ledger");
    }
}

/// Writes `bytes` to `path` through a staging file of this call's own,
/// renamed into place; nothing if the file already holds them.
fn write_staged(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    if std::fs::read(path).is_ok_and(|existing| existing == bytes) {
        return Ok(());
    }
    let staging = path.with_extension(format!(
        "{}.{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&staging, bytes)?;
    std::fs::rename(&staging, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&staging);
    })
}

/// The ledger's text: one `[[import]]` table per entry.
fn ledger_toml(called: &[UnimplementedCall]) -> String {
    let quote = |s: &str| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""));
    let mut text = String::from(
        "# Unimplemented imports the title called, most called first.\n\
         # Written by `victory run --execute`; ranked by `cargo xtask todo`.\n",
    );
    for c in called {
        text += &format!(
            "\n[[import]]\ndll = {}\nsymbol = {}\ncalls = {}\nfirst_caller = {}\n",
            quote(&c.dll),
            quote(&c.symbol),
            c.calls,
            quote(&c.first_caller)
        );
    }
    text
}

/// The crash report's lines for the unimplemented imports called so far
/// (any of them may have returned something the title did not expect).
pub(crate) fn report_lines() -> String {
    const SHOWN: usize = 16;
    let Some(called) = unimplemented_calls() else {
        return String::new();
    };
    let mut lines: String = called
        .iter()
        .take(SHOWN)
        .map(|c| {
            format!(
                "  unimplemented: {}!{} x{} (first from {})\n",
                c.dll, c.symbol, c.calls, c.first_caller
            )
        })
        .collect();
    if called.len() > SHOWN {
        lines += &format!("  unimplemented: {} more\n", called.len() - SHOWN);
    }
    lines
}

const THUNK_SIZE: usize = 32;

/// Executable memory holding one stub per unresolved import.
pub(crate) struct ThunkBlock {
    base: *mut u8,
    len: usize,
    used: usize,
}

impl ThunkBlock {
    /// Reserves room for `count` stubs (writable until [`Self::seal`]).
    pub(crate) fn new(count: usize) -> Result<Self> {
        let len = (count.max(1) * THUNK_SIZE).next_multiple_of(4096);
        // SAFETY: fresh anonymous allocation; no existing memory is touched.
        let base = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                len,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE,
            )
        } as *mut u8;
        if base.is_null() {
            bail!(
                "VirtualAlloc for import stubs failed: {}",
                std::io::Error::last_os_error()
            );
        }
        Ok(ThunkBlock { base, len, used: 0 })
    }

    /// Emits a stub for `site` and returns its address.
    pub(crate) fn emit(&mut self, site: &'static ImportSite) -> Result<usize> {
        if self.used + THUNK_SIZE > self.len {
            bail!("import stub block is full");
        }
        let mut code = [0xCC_u8; THUNK_SIZE]; // int3 padding
        code[0..4].copy_from_slice(&[0x48, 0x8B, 0x14, 0x24]);
        code[4..6].copy_from_slice(&[0x48, 0xB9]);
        code[6..14].copy_from_slice(&(site as *const ImportSite as u64).to_le_bytes());
        code[14..16].copy_from_slice(&[0x48, 0xB8]);
        let handler = unimplemented_import as extern "system" fn(&'static ImportSite, usize) -> u64;
        code[16..24].copy_from_slice(&(handler as *const () as u64).to_le_bytes());
        code[24..26].copy_from_slice(&[0xFF, 0xE0]);
        // SAFETY: `used + THUNK_SIZE <= len`, and the block is still writable.
        let at = unsafe {
            let at = self.base.add(self.used);
            std::ptr::copy_nonoverlapping(code.as_ptr(), at, THUNK_SIZE);
            at
        };
        self.used += THUNK_SIZE;
        Ok(at as usize)
    }

    /// Makes the block read+execute. The block is intentionally leaked:
    /// title code may hold these addresses for the life of the process.
    pub(crate) fn seal(self) -> Result<()> {
        let mut old = 0;
        // SAFETY: `base..base+len` is the allocation made in `new`.
        let ok = unsafe { VirtualProtect(self.base as _, self.len, PAGE_EXECUTE_READ, &mut old) };
        if ok == 0 {
            bail!(
                "VirtualProtect for import stubs failed: {}",
                std::io::Error::last_os_error()
            );
        }
        // SAFETY: flushing our own process's instruction cache for our own block.
        unsafe { FlushInstructionCache(GetCurrentProcess(), self.base as _, self.len) };
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// In a child process of the next test: calls an unimplemented import
    /// that traps, with the crash log where `VICTORY_TRAP_LOG` says.
    #[test]
    fn trap_child() {
        let Some(log) = std::env::var_os("VICTORY_TRAP_LOG") else {
            return;
        };
        let log = std::path::PathBuf::from(log);
        crate::log_crashes_to(log.clone());
        save_unimplemented_to(log.with_file_name("unimplemented.toml"));
        let site = ImportSite::leak("probe.dll", "Missing", UnimplementedPolicy::Trap);
        unimplemented_import(site, trap_child as *const () as usize);
    }

    #[test]
    fn a_trapped_import_is_in_the_crash_log_and_the_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("crashes.log");
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "thunks::tests::trap_child", "--nocapture"])
            .env("VICTORY_TRAP_LOG", &log)
            .output()
            .unwrap();
        assert!(!out.status.success(), "the trap ends the process");
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        let named = text.contains("VIctory: unimplemented import probe.dll!Missing called from ");
        assert!(named, "crash log: {text:?}");
        assert!(
            text.contains("  unimplemented: probe.dll!Missing x1 (first from "),
            "crash log: {text:?}"
        );
        let ledger = std::fs::read_to_string(dir.path().join("unimplemented.toml")).unwrap();
        let ledger: toml::Table = ledger.parse().unwrap();
        let import = &ledger["import"][0];
        assert_eq!(import["symbol"].as_str(), Some("Missing"));
        assert_eq!(import["calls"].as_integer(), Some(1));
    }

    /// Calls a generated stub the way title code does: through a `call`.
    #[test]
    fn a_stub_counts_calls_and_names_its_first_caller() {
        let site = ImportSite::leak("probe.dll", "CountedZero", UnimplementedPolicy::ReturnZero);
        let mut block = ThunkBlock::new(1).unwrap();
        let stub = block.emit(site).unwrap();
        block.seal().unwrap();
        // SAFETY: `stub` is sealed code that takes no arguments and returns
        // 0 in rax, per the Windows x64 convention.
        let stub: extern "system" fn() -> u64 = unsafe { std::mem::transmute(stub) };
        assert_eq!(stub(), 0);
        assert_eq!(stub(), 0);
        assert_eq!(site.calls.load(Ordering::Relaxed), 2);
        let caller = site.first_caller.load(Ordering::Relaxed);
        let here = a_stub_counts_calls_and_names_its_first_caller as *const () as usize;
        // The return address lies in this test, after the function's start.
        assert!(
            caller > here && caller - here < 0x1000,
            "{caller:#x} vs {here:#x}"
        );
        let called = unimplemented_calls().unwrap();
        let entry = called.iter().find(|c| c.symbol == "CountedZero").unwrap();
        assert_eq!(entry.calls, 2);
        assert!(entry.first_caller.contains("+0x"), "{}", entry.first_caller);
    }

    #[test]
    fn the_ledger_is_toml_most_called_first() {
        let call = |symbol: &str, calls| UnimplementedCall {
            dll: "a\"b.dll".into(),
            symbol: symbol.into(),
            calls,
            first_caller: r"x\y.exe+0x10".into(),
        };
        let text = ledger_toml(&[call("Hot", 9), call("Cold", 1)]);
        let table: toml::Table = text.parse().unwrap();
        let imports = table["import"].as_array().unwrap();
        assert_eq!(imports[0]["symbol"].as_str(), Some("Hot"));
        assert_eq!(imports[0]["dll"].as_str(), Some("a\"b.dll"));
        assert_eq!(imports[1]["first_caller"].as_str(), Some(r"x\y.exe+0x10"));
    }
}
