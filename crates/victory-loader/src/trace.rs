//! The HLE call trace: what the title asked of our runtime, in order.
//!
//! There is no single dispatch point for HLE calls: the loader writes each
//! implementation's address straight into the title's import table. So the
//! trace lives in the binding layer. With `VICTORY_HLE_TRACE` set, every
//! import bound to our runtime (and, with `VICTORY_HLE_TRACE_HOST=1`, every
//! host pass-through too: the waits and events of a hang are host calls)
//! is bound to a 32-byte stub instead, which enters a shared call body
//! with the site in `r10`:
//!
//! ```text
//! mov r10, <&'static TraceSite>   ; 49 BA imm64
//! mov r11, <call body>            ; 49 BB imm64
//! jmp r11                         ; 41 FF E3
//! ```
//!
//! The body records the call (its four register arguments and caller),
//! copies the caller's first twelve stack arguments into its own frame,
//! calls the implementation, records the result and returns. It has unwind
//! data, so stack walks and exceptions pass through it as through any
//! function. What it cannot pass on: more than sixteen arguments, and
//! functions that look at their own caller's frame (`setjmp`, `__chkstk`,
//! `RtlCaptureContext`), which are never traced.
//!
//! Each call and each return is an event in a fixed ring of `RING` slots
//! (one relaxed atomic index, a stamp per slot; no locks, no allocation).
//! A background thread turns events into lines of the trace file every
//! 50 ms; the crash bundle reads the last calls straight from the ring.
//! `VICTORY_HLE_TRACE=ring` keeps only the ring, for crash bundles.
//!
//! The file is ndjson, one line per call when it starts and one per return
//! (a call that never returns, the one a thread hangs in, has no second
//! line):
//!
//! ```text
//! {"seq":7,"t":1520,"thread":4242,"dll":"xmem.dll","fn":"XMemAlloc","args":[16,0,0,0],"caller":"title.exe+0x1234"}
//! {"seq":7,"t":1523,"thread":4242,"ret":1310720}
//! ```
//!
//! `t` is microseconds since the trace started, `args` the raw register
//! arguments (no handler declares a count, so always four), `seq` joins a
//! return to its call. `{"lost":n}` says the writer fell a lap behind.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering, fence};
use std::sync::{Mutex, OnceLock};

use windows_sys::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;

use crate::Provenance;

/// Turns the trace on: a file outside the repository, or `ring`.
pub const TRACE_ENV: &str = "VICTORY_HLE_TRACE";
/// `1`: host pass-through imports are traced too.
pub const HOST_ENV: &str = "VICTORY_HLE_TRACE_HOST";

/// One traced import. `target` stays first: the call body reads it at
/// offset 0. Leaked: title code holds its stub for the life of the process.
#[repr(C)]
pub(crate) struct TraceSite {
    target: usize,
    dll: String,
    name: String,
}

/// Every site made, to check a pointer read back from the ring.
static SITES: Mutex<Vec<usize>> = Mutex::new(Vec::new());

/// Events kept; a power of two.
const RING: usize = 1 << 16;

/// One event. `site` is 0 for a return; `values` are the four register
/// arguments and the caller for a call, the call's index and the result
/// for a return. `stamp` is the event's index plus one once it is whole,
/// 0 while it is written.
struct Slot {
    stamp: AtomicU64,
    site: AtomicU64,
    ticks: AtomicU64,
    thread: AtomicU64,
    values: [AtomicU64; 5],
}

impl Slot {
    const fn new() -> Slot {
        Slot {
            stamp: AtomicU64::new(0),
            site: AtomicU64::new(0),
            ticks: AtomicU64::new(0),
            thread: AtomicU64::new(0),
            values: [const { AtomicU64::new(0) }; 5],
        }
    }
}

static EVENTS: [Slot; RING] = [const { Slot::new() }; RING];
static HEAD: AtomicU64 = AtomicU64::new(0);

fn ticks() -> u64 {
    let mut now = 0;
    // SAFETY: writes the counter into `now`.
    unsafe { QueryPerformanceCounter(&mut now) };
    now as u64
}

fn record(site: u64, values: [u64; 5]) -> u64 {
    let i = HEAD.fetch_add(1, Ordering::Relaxed);
    let slot = &EVENTS[i as usize & (RING - 1)];
    slot.stamp.store(0, Ordering::Relaxed);
    fence(Ordering::Release);
    slot.site.store(site, Ordering::Relaxed);
    slot.ticks.store(ticks(), Ordering::Relaxed);
    // SAFETY: a plain query of the calling thread.
    let thread = unsafe { GetCurrentThreadId() };
    slot.thread.store(u64::from(thread), Ordering::Relaxed);
    for (slot, value) in slot.values.iter().zip(values) {
        slot.store(value, Ordering::Relaxed);
    }
    slot.stamp.store(i + 1, Ordering::Release);
    i
}

/// An event read back.
#[derive(Debug, Clone, Copy)]
struct Event {
    index: u64,
    site: usize,
    ticks: u64,
    thread: u32,
    values: [u64; 5],
}

enum Read {
    Event(Event),
    /// Still being written (or not yet).
    Pending,
    /// Overwritten by a later lap.
    Lost,
}

fn read(index: u64) -> Read {
    let slot = &EVENTS[index as usize & (RING - 1)];
    let stamp = slot.stamp.load(Ordering::Acquire);
    if stamp < index + 1 {
        return Read::Pending;
    }
    if stamp > index + 1 {
        return Read::Lost;
    }
    let event = Event {
        index,
        site: slot.site.load(Ordering::Relaxed) as usize,
        ticks: slot.ticks.load(Ordering::Relaxed),
        thread: slot.thread.load(Ordering::Relaxed) as u32,
        values: std::array::from_fn(|i| slot.values[i].load(Ordering::Relaxed)),
    };
    fence(Ordering::Acquire);
    if slot.stamp.load(Ordering::Relaxed) != stamp {
        return Read::Lost;
    }
    Read::Event(event)
}

/// The call body's first call: records the call, returns its index.
extern "system" fn enter(
    site: &'static TraceSite,
    args: *const [u64; 4],
    frame: *const u64,
) -> u64 {
    // SAFETY: the body passes its saved register arguments and the
    // caller's stack pointer, whose first word is the return address.
    let (args, caller) = unsafe { (*args, *frame) };
    let site = std::ptr::from_ref(site) as u64;
    record(site, [args[0], args[1], args[2], args[3], caller])
}

/// The call body's second call: records the return, passes `ret` back.
extern "system" fn leave(call: u64, ret: u64) -> u64 {
    record(0, [call, ret, 0, 0, 0]);
    ret
}

unsafe extern "C" {
    fn victory_trace_body();
}

// The frame, from rsp after the prologue: 0x20 home space for the calls
// made; 0x20..0x80 the caller's stack arguments 5 to 16, copied; 0x80 the
// four register arguments; 0xa0 xmm0-3 (xmm0 again for the result); 0xe0
// the call's index; 0xe8 the site. 0xf8 keeps rsp 16-byte aligned (the
// return address made it 8 off). The caller's stack arguments start at
// 0xf8 + 8 (its return address) + 0x20 (its home space) = 0x120.
std::arch::global_asm!(
    ".text",
    ".p2align 4",
    ".globl victory_trace_body",
    "victory_trace_body:",
    ".seh_proc victory_trace_body",
    "sub rsp, 0xf8",
    ".seh_stackalloc 0xf8",
    ".seh_endprologue",
    "mov [rsp + 0x80], rcx",
    "mov [rsp + 0x88], rdx",
    "mov [rsp + 0x90], r8",
    "mov [rsp + 0x98], r9",
    "movdqu [rsp + 0xa0], xmm0",
    "movdqu [rsp + 0xb0], xmm1",
    "movdqu [rsp + 0xc0], xmm2",
    "movdqu [rsp + 0xd0], xmm3",
    "mov [rsp + 0xe8], r10",
    "mov rcx, r10",
    "lea rdx, [rsp + 0x80]",
    "lea r8, [rsp + 0xf8]",
    "call {enter}",
    "mov [rsp + 0xe0], rax",
    "mov rax, [rsp + 0x120]",
    "mov [rsp + 0x20], rax",
    "mov rax, [rsp + 0x128]",
    "mov [rsp + 0x28], rax",
    "mov rax, [rsp + 0x130]",
    "mov [rsp + 0x30], rax",
    "mov rax, [rsp + 0x138]",
    "mov [rsp + 0x38], rax",
    "mov rax, [rsp + 0x140]",
    "mov [rsp + 0x40], rax",
    "mov rax, [rsp + 0x148]",
    "mov [rsp + 0x48], rax",
    "mov rax, [rsp + 0x150]",
    "mov [rsp + 0x50], rax",
    "mov rax, [rsp + 0x158]",
    "mov [rsp + 0x58], rax",
    "mov rax, [rsp + 0x160]",
    "mov [rsp + 0x60], rax",
    "mov rax, [rsp + 0x168]",
    "mov [rsp + 0x68], rax",
    "mov rax, [rsp + 0x170]",
    "mov [rsp + 0x70], rax",
    "mov rax, [rsp + 0x178]",
    "mov [rsp + 0x78], rax",
    "mov rcx, [rsp + 0x80]",
    "mov rdx, [rsp + 0x88]",
    "mov r8, [rsp + 0x90]",
    "mov r9, [rsp + 0x98]",
    "movdqu xmm0, [rsp + 0xa0]",
    "movdqu xmm1, [rsp + 0xb0]",
    "movdqu xmm2, [rsp + 0xc0]",
    "movdqu xmm3, [rsp + 0xd0]",
    "mov r10, [rsp + 0xe8]",
    "call qword ptr [r10]",
    "movdqu [rsp + 0xa0], xmm0",
    "mov rcx, [rsp + 0xe0]",
    "mov rdx, rax",
    "call {leave}",
    "movdqu xmm0, [rsp + 0xa0]",
    "add rsp, 0xf8",
    "ret",
    ".seh_endproc",
    enter = sym enter,
    leave = sym leave,
);

/// Imports that look at their caller's frame or use their own calling
/// convention; through the call body they would see its frame instead.
const NEVER: &[&str] = &[
    "_setjmp",
    "setjmp",
    "longjmp",
    "__chkstk",
    "_chkstk",
    "__alloca",
    "_alloca",
    "RtlCaptureContext",
    "RtlRestoreContext",
    "RtlUnwind",
    "__C_specific_handler",
    "__CxxFrameHandler",
    "__GSHandlerCheck",
];

struct Config {
    host: bool,
}

static CONFIG: OnceLock<Option<Config>> = OnceLock::new();

/// Whether an import bound to `provenance` goes through the trace. The
/// first call reads the environment and starts the file writer.
pub(crate) fn wanted(provenance: Provenance, symbol: &str) -> bool {
    let Some(config) = CONFIG.get_or_init(start) else {
        return false;
    };
    let kind = match provenance {
        Provenance::Hle => true,
        Provenance::Host => config.host,
        Provenance::Title => false,
    };
    kind && !NEVER.iter().any(|n| symbol.starts_with(n))
}

fn start() -> Option<Config> {
    let value = std::env::var_os(TRACE_ENV).filter(|v| !v.is_empty())?;
    let host = std::env::var_os(HOST_ENV).is_some_and(|v| v == "1");
    if value != "ring" {
        let file = PathBuf::from(&value);
        match open(&file) {
            Ok(out) => {
                tracing::info!(file = %file.display(), host, "HLE call trace");
                let writer = Writer::new(out);
                *WRITER.lock().unwrap_or_else(|e| e.into_inner()) = Some(writer);
                let spawned = std::thread::Builder::new()
                    .name("hle-trace".into())
                    .spawn(|| {
                        loop {
                            std::thread::sleep(std::time::Duration::from_millis(50));
                            if let Some(w) =
                                WRITER.lock().unwrap_or_else(|e| e.into_inner()).as_mut()
                            {
                                w.drain();
                            }
                        }
                    });
                if let Err(error) = spawned {
                    tracing::warn!(%error, "HLE call trace: no writer thread; written at exit only");
                }
            }
            Err(error) => {
                tracing::error!(file = %file.display(), %error, "HLE call trace: keeping the ring only");
            }
        }
    } else {
        tracing::info!(host, "HLE call trace: ring only");
    }
    Some(Config { host })
}

fn open(file: &std::path::Path) -> anyhow::Result<std::fs::File> {
    if let Some(dir) = file.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    victory_core::ensure_outside_repo(file)?;
    Ok(std::fs::File::create(file)?)
}

/// A new site for `dll!name` at `target`, and the stub's code for it.
pub(crate) fn stub(dll: &str, name: &str, target: usize) -> [u8; 32] {
    let site: &'static TraceSite = Box::leak(Box::new(TraceSite {
        target,
        dll: dll.to_owned(),
        name: name.to_owned(),
    }));
    let site = std::ptr::from_ref(site) as usize;
    SITES.lock().unwrap_or_else(|e| e.into_inner()).push(site);
    let mut code = [0xCC_u8; 32];
    code[0..2].copy_from_slice(&[0x49, 0xBA]);
    code[2..10].copy_from_slice(&(site as u64).to_le_bytes());
    code[10..12].copy_from_slice(&[0x49, 0xBB]);
    let body = victory_trace_body as *const () as u64;
    code[12..20].copy_from_slice(&body.to_le_bytes());
    code[20..23].copy_from_slice(&[0x41, 0xFF, 0xE3]);
    code
}

/// The site an event names, if it is one of ours (a torn read is not).
fn site_of(site: usize, known: &HashSet<usize>) -> Option<&'static TraceSite> {
    // SAFETY: every address in `known` is a leaked, never freed site.
    known
        .contains(&site)
        .then(|| unsafe { &*(site as *const TraceSite) })
}

fn known_sites() -> Option<HashSet<usize>> {
    let sites = SITES.try_lock().ok()?;
    Some(sites.iter().copied().collect())
}

/// Microseconds from `start` to `ticks`.
fn micros(ticks: u64, start: u64, frequency: u64) -> u64 {
    (u128::from(ticks.saturating_sub(start)) * 1_000_000 / u128::from(frequency.max(1))) as u64
}

static WRITER: Mutex<Option<Writer>> = Mutex::new(None);

struct Writer {
    out: std::io::BufWriter<std::fs::File>,
    /// The next event to write.
    next: u64,
    /// Times `next` was found still being written.
    stuck: u32,
    lost: u64,
    start: u64,
    frequency: u64,
    sites: HashSet<usize>,
    callers: HashMap<u64, String>,
}

impl Writer {
    fn new(out: std::fs::File) -> Writer {
        let mut frequency = 0;
        // SAFETY: writes the frequency into `frequency`.
        unsafe { QueryPerformanceFrequency(&mut frequency) };
        Writer {
            out: std::io::BufWriter::with_capacity(1 << 20, out),
            next: HEAD.load(Ordering::Acquire),
            stuck: 0,
            lost: 0,
            start: ticks(),
            frequency: frequency as u64,
            sites: HashSet::new(),
            callers: HashMap::new(),
        }
    }

    /// Writes every whole event up to the ring's head.
    fn drain(&mut self) {
        let head = HEAD.load(Ordering::Acquire);
        if head - self.next > RING as u64 {
            self.lost += head - RING as u64 - self.next;
            self.next = head - RING as u64;
        }
        while self.next < head {
            match read(self.next) {
                Read::Event(event) => {
                    self.write(&event);
                    self.stuck = 0;
                }
                Read::Lost => self.lost += 1,
                // A writer that stopped mid-event (ended, suspended) is
                // waited for a second, then passed.
                Read::Pending if self.stuck < 20 => {
                    self.stuck += 1;
                    break;
                }
                Read::Pending => {
                    self.lost += 1;
                    self.stuck = 0;
                }
            }
            self.next += 1;
        }
        if self.lost > 0 {
            let _ = writeln!(self.out, "{{\"lost\":{}}}", self.lost);
            self.lost = 0;
        }
        let _ = self.out.flush();
    }

    fn write(&mut self, e: &Event) {
        let t = micros(e.ticks, self.start, self.frequency);
        if e.site == 0 {
            let [call, ret, ..] = e.values;
            let _ = writeln!(
                self.out,
                "{{\"seq\":{call},\"t\":{t},\"thread\":{},\"ret\":{ret}}}",
                e.thread
            );
            return;
        }
        if !self.sites.contains(&e.site)
            && let Some(sites) = known_sites()
        {
            self.sites = sites;
        }
        let Some(site) = site_of(e.site, &self.sites) else {
            self.lost += 1;
            return;
        };
        let [a, b, c, d, caller] = e.values;
        let at = self
            .callers
            .entry(caller)
            .or_insert_with(|| crate::locate(caller as usize));
        let _ = writeln!(
            self.out,
            "{{\"seq\":{},\"t\":{t},\"thread\":{},\"dll\":{},\"fn\":{},\"args\":[{a},{b},{c},{d}],\"caller\":{}}}",
            e.index,
            e.thread,
            json(&site.dll),
            json(&site.name),
            json(at)
        );
    }
}

fn json(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Writes what the trace file lacks, if no other thread is writing it (on
/// a crash or at exit; the writer thread may be the one that stopped).
pub(crate) fn flush() {
    if let Ok(mut writer) = WRITER.try_lock()
        && let Some(w) = writer.as_mut()
    {
        w.drain();
    }
}

/// The last `n` calls in the ring, oldest first, one line each, for the
/// crash bundle; a note when the trace is off.
pub(crate) fn recent_lines(n: usize) -> String {
    if !matches!(CONFIG.get(), Some(Some(_))) {
        return format!("  (no HLE call trace: set {TRACE_ENV}=ring or a file to record one)\n");
    }
    let Some(sites) = known_sites() else {
        return "  (the trace's site list is held by another thread)\n".into();
    };
    let head = HEAD.load(Ordering::Acquire);
    let oldest = head.saturating_sub(RING as u64);
    let mut calls = Vec::new();
    let mut returns = HashMap::new();
    let mut index = head;
    while index > oldest && calls.len() < n {
        index -= 1;
        let Read::Event(event) = read(index) else {
            continue;
        };
        if event.site == 0 {
            returns.insert(event.values[0], event.values[1]);
        } else if let Some(site) = site_of(event.site, &sites) {
            calls.push((event, site));
        }
    }
    let start = calls.last().map_or(0, |(e, _)| e.ticks);
    let mut frequency = 0;
    // SAFETY: writes the frequency into `frequency`.
    unsafe { QueryPerformanceFrequency(&mut frequency) };
    let mut text = String::new();
    for (e, site) in calls.iter().rev() {
        let [a, b, c, d, caller] = e.values;
        let ret = match returns.get(&e.index) {
            Some(ret) => format!("ret={ret:#x}"),
            None => "not returned".into(),
        };
        text += &format!(
            "  #{} t=+{}us thread={} {}!{} args=[{a:#x}, {b:#x}, {c:#x}, {d:#x}] {ret} from {}\n",
            e.index,
            micros(e.ticks, start, frequency as u64),
            e.thread,
            site.dll,
            site.name,
            crate::locate(caller as usize)
        );
    }
    if text.is_empty() {
        text = "  (no HLE calls recorded)\n".into();
    }
    text
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::thunks::ThunkBlock;

    /// A callable stub for `target`, traced as `test.dll!name`.
    pub(crate) fn traced(name: &str, target: usize) -> usize {
        let mut block = ThunkBlock::new(1).unwrap();
        let stub = block.emit_code(&stub("test.dll", name, target)).unwrap();
        block.seal().unwrap();
        stub
    }

    /// The calls of `name` this thread made, with their returns.
    fn calls_of(name: &str) -> Vec<([u64; 5], Option<u64>)> {
        // SAFETY: a plain query of the calling thread.
        let me = unsafe { GetCurrentThreadId() };
        // Waits for the list: tests running in parallel add sites to it.
        let sites: HashSet<usize> = SITES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .copied()
            .collect();
        let head = HEAD.load(Ordering::Acquire);
        let events: Vec<Event> = (head.saturating_sub(RING as u64)..head)
            .filter_map(|i| match read(i) {
                Read::Event(e) if e.thread == me => Some(e),
                _ => None,
            })
            .collect();
        events
            .iter()
            .filter(|e| site_of(e.site, &sites).is_some_and(|s| s.name == name))
            .map(|call| {
                let ret = events
                    .iter()
                    .find(|r| r.site == 0 && r.values[0] == call.index)
                    .map(|r| r.values[1]);
                (call.values, ret)
            })
            .collect()
    }

    #[allow(clippy::too_many_arguments)]
    extern "system" fn weigh(
        a: u64,
        b: u64,
        c: u64,
        d: u64,
        e: u64,
        f: u64,
        g: u64,
        h: u64,
        i: u64,
        j: u64,
        k: u64,
        l: u64,
        m: u64,
        n: u64,
        o: u64,
        p: u64,
    ) -> u64 {
        [a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p]
            .iter()
            .enumerate()
            .map(|(i, v)| v << (4 * i))
            .sum()
    }

    #[test]
    fn a_traced_call_passes_sixteen_arguments_and_its_result() {
        type Weigh = extern "system" fn(
            u64,
            u64,
            u64,
            u64,
            u64,
            u64,
            u64,
            u64,
            u64,
            u64,
            u64,
            u64,
            u64,
            u64,
            u64,
            u64,
        ) -> u64;
        let stub = traced("Weigh", weigh as *const () as usize);
        // SAFETY: the stub forwards to `weigh`, which has this signature.
        let call: Weigh = unsafe { std::mem::transmute(stub) };
        let got = call(1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 1);
        assert_eq!(got, 0x1fed_cba9_8765_4321);
        let calls = calls_of("Weigh");
        assert_eq!(calls.len(), 1);
        let (values, ret) = calls[0];
        assert_eq!(values[..4], [1, 2, 3, 4]);
        assert_eq!(ret, Some(got));
        // The caller is in this test.
        let here = a_traced_call_passes_sixteen_arguments_and_its_result as *const () as u64;
        assert!(
            values[4] > here && values[4] - here < 0x1000,
            "{:#x}",
            values[4]
        );
    }

    extern "system" fn scale(x: f64, y: f32, n: u32, z: f64) -> f64 {
        x * f64::from(y) + f64::from(n) + z
    }

    #[test]
    fn a_traced_call_keeps_float_arguments_and_results() {
        type Scale = extern "system" fn(f64, f32, u32, f64) -> f64;
        let stub = traced("Scale", scale as *const () as usize);
        // SAFETY: the stub forwards to `scale`, which has this signature.
        let call: Scale = unsafe { std::mem::transmute(stub) };
        assert_eq!(call(1.5, 2.0, 3, 0.25), 6.25);
        assert_eq!(call(2.0, 0.5, 0, 0.0), 1.0);
        assert_eq!(calls_of("Scale").len(), 2);
    }

    /// Walks the stack from inside a traced call.
    extern "system" fn walk(out: *mut [usize; 16]) -> u16 {
        use windows_sys::Win32::System::Diagnostics::Debug::RtlCaptureStackBackTrace;
        // SAFETY: writes at most 16 frames into the caller's array.
        unsafe { RtlCaptureStackBackTrace(0, 16, out.cast(), std::ptr::null_mut()) }
    }

    #[test]
    #[inline(never)]
    fn stack_walks_pass_through_a_traced_call() {
        let stub = traced("Walk", walk as *const () as usize);
        // SAFETY: the stub forwards to `walk`, which has this signature.
        let call: extern "system" fn(*mut [usize; 16]) -> u16 =
            unsafe { std::mem::transmute(stub) };
        let mut frames = [0usize; 16];
        let n = usize::from(call(&mut frames));
        let here = stack_walks_pass_through_a_traced_call as *const () as usize;
        // walk, the call body, then this test: the unwinder found the body's
        // unwind data and went on past it.
        let found = frames[..n].iter().any(|&f| f > here && f - here < 0x1000);
        assert!(found, "{:x?}", &frames[..n]);
    }

    #[test]
    fn setjmp_and_title_imports_are_never_traced() {
        // Whether the trace is on depends on the environment; these are
        // off either way.
        assert!(!wanted(Provenance::Title, "Anything"));
        assert!(!wanted(Provenance::Hle, "__chkstk"));
        assert!(!wanted(Provenance::Host, "_setjmpex"));
    }

    /// In a child process of the next test: makes traced calls with the
    /// trace file where `VICTORY_HLE_TRACE` says.
    #[test]
    fn trace_child() {
        if std::env::var_os("VICTORY_TRACE_CHILD").is_none() {
            return;
        }
        assert!(wanted(Provenance::Hle, "Weigh"));
        let stub = traced("Half", half as *const () as usize);
        // SAFETY: the stub forwards to `half`.
        let call: extern "system" fn(u64) -> u64 = unsafe { std::mem::transmute(stub) };
        for n in [10, 20, 30] {
            assert_eq!(call(n), n / 2);
        }
        flush();
    }

    extern "system" fn half(n: u64) -> u64 {
        n / 2
    }

    #[test]
    fn the_trace_file_has_a_line_per_call_and_per_return() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("trace.ndjson");
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "trace::tests::trace_child", "--nocapture"])
            .env("VICTORY_TRACE_CHILD", "1")
            .env(TRACE_ENV, &file)
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        let text = std::fs::read_to_string(&file).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        let calls: Vec<&&str> = lines
            .iter()
            .filter(|l| l.contains("\"fn\":\"Half\""))
            .collect();
        assert_eq!(calls.len(), 3, "{text}");
        assert!(calls[0].contains("\"dll\":\"test.dll\"") && calls[0].contains("\"args\":[10,"));
        assert!(
            calls[0].contains("\"caller\":\"victory_loader-"),
            "{}",
            calls[0]
        );
        let returns: Vec<&&str> = lines.iter().filter(|l| l.contains("\"ret\":")).collect();
        assert!(returns.iter().any(|l| l.ends_with("\"ret\":15}")), "{text}");
        // Each return names its call's seq.
        let seq = calls[2]
            .split("\"seq\":")
            .nth(1)
            .unwrap()
            .split(',')
            .next()
            .unwrap();
        assert!(
            returns
                .iter()
                .any(|l| l.starts_with(&format!("{{\"seq\":{seq},"))),
            "{text}"
        );
    }

    #[test]
    fn microseconds_come_from_counter_ticks() {
        assert_eq!(micros(1_500, 500, 1_000_000), 1_000);
        assert_eq!(micros(10, 500, 1_000), 0);
        assert_eq!(json("a\"b\\\n"), "\"a\\\"b\\\\\\u000a\"");
    }
}
