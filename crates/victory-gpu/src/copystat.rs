//! Diagnostics: where the submission copies of title memory go (which
//! ranges, how often, of what memory) and what limits their rate, logged
//! with the frame-rate line. `VICTORY_COPY_MODE=plain` copies without the
//! mapped-page guard, `none` skips the copies (both for timing only).

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Counters since the last line.
pub(crate) enum C {
    Submissions,
    Copies,
    CopyBytes,
    Refreshes,
    RefreshBytes,
    Skipped,
    SkippedBytes,
    Runs,
    RunUs,
    Queries,
    QueryUs,
    MoveUs,
    Unmapped,
    /// Write-watch askings and their time.
    Watches,
    WatchUs,
    /// Bytes of watched ranges checked, and of those found unchanged.
    Checked,
    Unchanged,
    /// Copies that met an unmapped page.
    Fallbacks,
    /// Writes taken by the guard on view memory.
    GuardFaults,
    /// Texture bytes read after a watched write; hashed; detiled after a
    /// hash change.
    TexWatched,
    TexHashed,
    TexChanged,
    /// Staging buffers copied into host textures (and their bytes), and
    /// those not copied because the host texture already held them.
    TexUploads,
    TexUploadBytes,
    TexSkips,
    TexSkipBytes,
    /// Of submission preparing, microseconds: ordering the lists and
    /// copies; the lists' submission-time steps (refreshes, texture
    /// checks, copies); state preludes; bindless slots (of them, plans
    /// made again); snapshots of what shaders may write; retired objects.
    PrepOrderUs,
    PrepStepsUs,
    PrepPreludeUs,
    PrepBindlessUs,
    PrepPlanUs,
    PrepSnapshotUs,
    PrepRetiredUs,
    /// Executions waited for (submissions are split at copies and waits).
    Batches,
    /// Microseconds blocked in title waits (`Wait32/64BitValueX`).
    TitleWaitUs,
    /// Texture checks made, and those a batch earlier in the submission
    /// already made.
    TexChecks,
    TexRechecksSaved,
    /// Microseconds recording threads waited for compute pipelines, and
    /// pipelines made on the waiting thread.
    ComputeWaitUs,
    ComputeMadeHere,
    /// Bindless plans made again, the slots they went through, and of their
    /// time, microseconds checking memory is committed and finding copies.
    Plans,
    PlanSlots,
    PlanQueryUs,
    PlanFindUs,
    /// Plans made again because a slot was written (else memory or placed
    /// textures changed).
    PlansForSlots,
    /// Of the write-watch time, microseconds in the kernel's answer and in
    /// guarding view memory, and the 64 KiB pages asked about.
    WatchKernelUs,
    WatchGuardUs,
    WatchPages,
}
const COUNTERS: usize = 47;
static COUNT: [AtomicU64; COUNTERS] = [const { AtomicU64::new(0) }; COUNTERS];

pub(crate) fn add(c: C, n: u64) {
    COUNT[c as usize].fetch_add(n, Relaxed);
}

/// Microseconds since `started`.
pub(crate) fn since(started: std::time::Instant) -> u64 {
    started.elapsed().as_micros() as u64
}

/// How copies run: 0 guarded (as always), 1 plain, 2 not at all.
pub(crate) fn mode() -> u8 {
    static MODE: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| match std::env::var("VICTORY_COPY_MODE").as_deref() {
        Ok("plain") => 1,
        Ok("none") => 2,
        _ => 0,
    })
}

/// Per source range (start, length): copies, bytes, microseconds, memory type.
type Ranges = HashMap<(usize, usize), (u64, u64, u64, u32)>;
static RANGES: Mutex<Option<Ranges>> = Mutex::new(None);

/// A copy of `len` bytes from `src` (memory type `kind`) took `us`.
pub(crate) fn range(src: usize, len: usize, us: u64, kind: u32) {
    let mut map = RANGES.lock().unwrap_or_else(|e| e.into_inner());
    let e = map
        .get_or_insert_with(HashMap::new)
        .entry((src, len))
        .or_insert((0, 0, 0, kind));
    e.0 += 1;
    e.1 += len as u64;
    e.2 += us;
}

fn page_faults() -> u64 {
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut c = PROCESS_MEMORY_COUNTERS {
        cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    // SAFETY: our own process and a sized structure.
    unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) };
    u64::from(c.PageFaultCount)
}

/// Logs the counters per frame over `frames` frames, and the ranges that
/// took most bytes.
pub(crate) fn log(frames: f64) {
    static FAULTS: AtomicU64 = AtomicU64::new(0);
    let faults = page_faults();
    let faults = faults - FAULTS.swap(faults, Relaxed).min(faults);
    let c: Vec<f64> = COUNT
        .iter()
        .map(|a| a.swap(0, Relaxed) as f64 / frames)
        .collect();
    let mb = |v: f64| v / 1e6;
    let ms = |v: f64| v / 1e3;
    tracing::info!(
        "copies per frame: {:.1} submissions; {:.0} title copies {:.1} MB; {:.0} refreshes {:.1} MB, \
         {:.0} skipped {:.1} MB; {:.0} runs {:.1} ms; {:.0} queries {:.2} ms, moves {:.2} ms, \
         {:.2} MB unmapped; {:.0} page faults; {:.0} watches {:.2} ms, {:.1} MB checked, {:.1} MB \
         unchanged; {:.1} fallbacks; {:.0} guard faults; textures {:.1} MB watched and \
         changed, {:.1} MB hashed, {:.1} MB of them changed",
        c[C::Submissions as usize],
        c[C::Copies as usize],
        mb(c[C::CopyBytes as usize]),
        c[C::Refreshes as usize],
        mb(c[C::RefreshBytes as usize]),
        c[C::Skipped as usize],
        mb(c[C::SkippedBytes as usize]),
        c[C::Runs as usize],
        ms(c[C::RunUs as usize]),
        c[C::Queries as usize],
        ms(c[C::QueryUs as usize]),
        ms(c[C::MoveUs as usize]),
        mb(c[C::Unmapped as usize]),
        faults as f64 / frames,
        c[C::Watches as usize],
        ms(c[C::WatchUs as usize]),
        mb(c[C::Checked as usize]),
        mb(c[C::Unchanged as usize]),
        c[C::Fallbacks as usize],
        c[C::GuardFaults as usize],
        mb(c[C::TexWatched as usize]),
        mb(c[C::TexHashed as usize]),
        mb(c[C::TexChanged as usize]),
    );
    tracing::info!(
        "submission preparing per frame: {:.1} batches; ordering {:.1} ms, list steps {:.1} ms, \
         preludes {:.1} ms, bindless {:.1} ms (plans made {:.1} ms), write snapshots {:.1} ms, \
         retired {:.1} ms; title waits {:.1} ms; {:.0} texture checks, {:.0} saved; compute \
         pipeline waits on recording threads {:.1} ms ({:.0} made there)",
        c[C::Batches as usize],
        ms(c[C::PrepOrderUs as usize]),
        ms(c[C::PrepStepsUs as usize]),
        ms(c[C::PrepPreludeUs as usize]),
        ms(c[C::PrepBindlessUs as usize]),
        ms(c[C::PrepPlanUs as usize]),
        ms(c[C::PrepSnapshotUs as usize]),
        ms(c[C::PrepRetiredUs as usize]),
        ms(c[C::TitleWaitUs as usize]),
        c[C::TexChecks as usize],
        c[C::TexRechecksSaved as usize],
        ms(c[C::ComputeWaitUs as usize]),
        c[C::ComputeMadeHere as usize],
    );
    tracing::info!(
        "bindless plans per frame: {:.1} made ({:.1} for written slots), {:.0} slots; \
         committed checks {:.1} ms, copies found {:.1} ms",
        c[C::Plans as usize],
        c[C::PlansForSlots as usize],
        c[C::PlanSlots as usize],
        ms(c[C::PlanQueryUs as usize]),
        ms(c[C::PlanFindUs as usize]),
    );
    tracing::info!(
        "write watch per frame: kernel {:.1} ms, view guards {:.1} ms, {:.0} pages asked",
        ms(c[C::WatchKernelUs as usize]),
        ms(c[C::WatchGuardUs as usize]),
        c[C::WatchPages as usize],
    );
    tracing::info!(
        "texture uploads per frame: {:.0} copied {:.1} MB, {:.0} skipped unchanged {:.1} MB",
        c[C::TexUploads as usize],
        mb(c[C::TexUploadBytes as usize]),
        c[C::TexSkips as usize],
        mb(c[C::TexSkipBytes as usize]),
    );
    let ranges = RANGES.lock().unwrap_or_else(|e| e.into_inner()).take();
    let mut ranges: Vec<_> = ranges.into_iter().flatten().collect();
    ranges.sort_by_key(|&(_, (_, bytes, _, _))| std::cmp::Reverse(bytes));
    for ((src, len), (n, bytes, us, _)) in ranges.into_iter().take(10) {
        use windows_sys::Win32::System::Memory::{MEMORY_BASIC_INFORMATION, VirtualQuery};
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        unsafe { VirtualQuery(src as *const _, &mut info, size_of_val(&info)) };
        let kind = info.Type;
        tracing::info!(
            "  copied from {src:#x}+{len:#x} (memory {kind:#x}): {:.1}x/frame, {:.2} MB/frame, {:.2} ms/frame",
            n as f64 / frames,
            bytes as f64 / 1e6 / frames,
            us as f64 / 1e3 / frames
        );
    }
}
