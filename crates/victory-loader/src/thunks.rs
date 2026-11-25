//! Stubs for imports nobody implements yet.
//!
//! Each unresolved import gets 32 bytes of generated code:
//!
//! ```text
//! mov rcx, <&'static ImportSite>   ; 48 B9 imm64
//! mov rdx, [rsp]                   ; 48 8B 14 24 (the caller)
//! mov rax, <unimplemented_import>  ; 48 B8 imm64
//! jmp rax                          ; FF E0
//! ```
//!
//! The jump keeps the title's return address on the stack, so the handler
//! returns straight to the caller. Bring-up then shows exactly which API to
//! implement next, rather than failing at load time.
//!
//! Every stub counts its calls and keeps its first caller. With
//! [`record_stub_hits_to`] the counts go to a ledger file (`hits`,
//! `dll!symbol`, first caller as `module+rva`, tab-separated, most hit
//! first), rewritten every second and when the title traps, crashes or
//! exits; `cargo xtask todo` ranks ledgers into the HLE to-do list.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

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
    /// Return address of the first call, 0 until there is one.
    pub first_caller: AtomicUsize,
}

impl ImportSite {
    /// A new site, leaked and listed for the stub ledger.
    pub(crate) fn leak(dll: &str, symbol: &str, policy: UnimplementedPolicy) -> &'static Self {
        let site: &'static Self = Box::leak(Box::new(ImportSite {
            dll: dll.to_owned(),
            symbol: symbol.to_owned(),
            policy,
            calls: AtomicU64::new(0),
            first_caller: AtomicUsize::new(0),
        }));
        sites().push(site);
        site
    }
}

static UNIMPLEMENTED_CALLS: AtomicU64 = AtomicU64::new(0);
static SITES: Mutex<Vec<&'static ImportSite>> = Mutex::new(Vec::new());
static LEDGER: OnceLock<PathBuf> = OnceLock::new();

fn sites() -> std::sync::MutexGuard<'static, Vec<&'static ImportSite>> {
    SITES.lock().unwrap_or_else(|e| e.into_inner())
}

/// Total number of calls that reached an unimplemented-import stub.
pub fn unimplemented_call_count() -> u64 {
    UNIMPLEMENTED_CALLS.load(Ordering::Relaxed)
}

/// One unimplemented import the title called.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StubHit {
    pub dll: String,
    pub symbol: String,
    pub hits: u64,
    /// Return address of the first call.
    pub first_caller: usize,
}

/// Every unimplemented import called so far, most hit first. Stubs for the
/// same `dll!symbol` in several images are summed (first caller: the first
/// image's that has one).
pub fn stub_hits() -> Vec<StubHit> {
    let mut hits: Vec<StubHit> = Vec::new();
    for site in sites().iter() {
        let calls = site.calls.load(Ordering::Relaxed);
        if calls == 0 {
            continue;
        }
        let caller = site.first_caller.load(Ordering::Relaxed);
        let same =
            |h: &&mut StubHit| h.dll.eq_ignore_ascii_case(&site.dll) && h.symbol == site.symbol;
        match hits.iter_mut().find(same) {
            Some(hit) => {
                hit.hits += calls;
                if hit.first_caller == 0 {
                    hit.first_caller = caller;
                }
            }
            None => hits.push(StubHit {
                dll: site.dll.clone(),
                symbol: site.symbol.clone(),
                hits: calls,
                first_caller: caller,
            }),
        }
    }
    rank(&mut hits);
    hits
}

fn rank(hits: &mut [StubHit]) {
    hits.sort_by(|a, b| {
        (b.hits.cmp(&a.hits))
            .then_with(|| a.dll.to_ascii_lowercase().cmp(&b.dll.to_ascii_lowercase()))
            .then_with(|| a.symbol.cmp(&b.symbol))
    });
}

/// The ledger's text: a header, then `hits<TAB>dll!symbol<TAB>module+rva`.
fn ledger_text(hits: &[StubHit], locate: impl Fn(usize) -> String) -> String {
    let mut text = String::from("# victory stub ledger v1\n# hits\timport\tfirst caller\n");
    for hit in hits {
        let caller = match hit.first_caller {
            0 => "?".to_owned(),
            at => locate(at),
        };
        text += &format!("{}\t{}!{}\t{caller}\n", hit.hits, hit.dll, hit.symbol);
    }
    text
}

/// Writes the stub ledger to `file`, replacing it whole (a reader never
/// sees half of one).
pub fn write_stub_ledger(file: &Path) -> std::io::Result<()> {
    let text = ledger_text(&stub_hits(), crate::locate);
    let partial = file.with_extension("tsv.partial");
    std::fs::write(&partial, text)?;
    std::fs::rename(&partial, file)
}

/// Keeps the stub ledger in `file`: written now, every second, and when
/// the title traps on a stub, crashes or [`flush_stub_ledger`] is called.
/// `file`'s folder must exist and lie outside the repository; only the
/// first call takes effect.
pub fn record_stub_hits_to(file: PathBuf) -> std::io::Result<()> {
    if let Some(parent) = file.parent().filter(|p| !p.as_os_str().is_empty()) {
        victory_core::ensure_outside_repo(parent).map_err(std::io::Error::other)?;
    }
    write_stub_ledger(&file)?;
    if LEDGER.set(file).is_ok() {
        std::thread::Builder::new()
            .name("victory-stub-ledger".into())
            .spawn(|| {
                loop {
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    flush_stub_ledger();
                }
            })?;
    }
    Ok(())
}

/// Rewrites the stub ledger now, if one is kept.
pub fn flush_stub_ledger() {
    if let Some(file) = LEDGER.get()
        && let Err(e) = write_stub_ledger(file)
    {
        tracing::warn!(file = %file.display(), "writing the stub ledger: {e}");
    }
}

extern "system" fn unimplemented_import(site: &'static ImportSite, caller: usize) -> u64 {
    UNIMPLEMENTED_CALLS.fetch_add(1, Ordering::Relaxed);
    let _ = site
        .first_caller
        .compare_exchange(0, caller, Ordering::Relaxed, Ordering::Relaxed);
    let previous = site.calls.fetch_add(1, Ordering::Relaxed);
    match site.policy {
        UnimplementedPolicy::ReturnZero => {
            if previous == 0 {
                tracing::warn!(dll = %site.dll, symbol = %site.symbol, "unimplemented import called, returning 0");
            }
            0
        }
        UnimplementedPolicy::Trap => {
            tracing::error!(dll = %site.dll, symbol = %site.symbol, "unimplemented import called");
            // Into the crash log too: `abort` ends the process without
            // running the handlers that write it.
            let (dll, symbol) = (&site.dll, &site.symbol);
            let text = format!("VIctory: unimplemented import {dll}!{symbol} called");
            crate::log_crash(&text);
            flush_stub_ledger();
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
        code[0..2].copy_from_slice(&[0x48, 0xB9]);
        code[2..10].copy_from_slice(&(site as *const ImportSite as u64).to_le_bytes());
        code[10..14].copy_from_slice(&[0x48, 0x8B, 0x14, 0x24]);
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
        crate::log_crashes_to(log.into());
        let site = ImportSite::leak("probe.dll", "Missing", UnimplementedPolicy::Trap);
        unimplemented_import(site, 0);
    }

    #[test]
    fn the_ledger_ranks_by_hits_and_names_the_first_caller() {
        let hit = |dll: &str, symbol: &str, hits, first_caller| StubHit {
            dll: dll.into(),
            symbol: symbol.into(),
            hits,
            first_caller,
        };
        let mut hits = vec![
            hit("b.dll", "Rare", 1, 0x10),
            hit("a.dll", "Hot", 40, 0x20),
            hit("A.dll", "Tied", 1, 0),
        ];
        rank(&mut hits);
        let text = ledger_text(&hits, |at| format!("title.exe+{at:#x}"));
        assert_eq!(
            text,
            "# victory stub ledger v1\n# hits\timport\tfirst caller\n\
             40\ta.dll!Hot\ttitle.exe+0x20\n\
             1\tA.dll!Tied\t?\n\
             1\tb.dll!Rare\ttitle.exe+0x10\n"
        );
    }

    /// Through a real stub: the call is counted, its caller kept, and the
    /// ledger written where asked.
    #[test]
    fn a_stub_call_reaches_the_ledger() {
        let site = ImportSite::leak("ledger.dll", "Probe", UnimplementedPolicy::ReturnZero);
        let mut block = ThunkBlock::new(1).unwrap();
        let stub = block.emit(site).unwrap();
        block.seal().unwrap();
        // SAFETY: the stub takes any arguments and returns a u64.
        let f: extern "system" fn(u64) -> u64 = unsafe { std::mem::transmute(stub) };
        assert_eq!(f(7), 0);
        assert_eq!(f(8), 0);
        assert_eq!(site.calls.load(Ordering::Relaxed), 2);
        // The return address is in this test, so in this executable.
        let caller = crate::locate(site.first_caller.load(Ordering::Relaxed));
        let exe = std::env::current_exe().unwrap();
        let exe = exe.file_name().unwrap().to_string_lossy().into_owned();
        assert!(caller.starts_with(&format!("{exe}+0x")), "caller {caller}");

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("stub-hits.tsv");
        write_stub_ledger(&file).unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        let line = text
            .lines()
            .find(|l| l.contains("ledger.dll!Probe"))
            .unwrap();
        let fields: Vec<_> = line.split('\t').collect();
        assert_eq!(fields[..2], ["2", "ledger.dll!Probe"]);
        assert!(fields[2].contains("+0x"), "first caller: {line}");
    }

    #[test]
    fn a_trapped_import_is_in_the_crash_log() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("crashes.log");
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "thunks::tests::trap_child", "--nocapture"])
            .env("VICTORY_TRAP_LOG", &log)
            .output()
            .unwrap();
        assert!(!out.status.success(), "the trap ends the process");
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        let named = text.contains("VIctory: unimplemented import probe.dll!Missing called");
        assert!(named, "crash log: {text:?}");
    }
}
