//! Command queues. Submitted lists run on the host GPU and are waited for,
//! so fence signals complete at once. A submission whose lists wait for
//! title memory runs on the queue's own thread instead (#700), with what
//! the title queues after it: the title goes on once it ran or waits, and
//! may submit what releases the wait.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use crate::object::{self, E_INVALIDARG, HRESULT, Iid, Object, QUEUE, S_OK, Table, This, guid};
use crate::resource::PAGEABLE_IIDS;

const QUEUE_IIDS: [Iid; 4] = [
    guid(
        0x0ec870a6,
        0x5d7e,
        0x4c22,
        [0x8c, 0xfc, 0x5b, 0xaa, 0xe0, 0x76, 0x16, 0xed],
    ),
    PAGEABLE_IIDS[0],
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct QueueDesc {
    kind: u32,
    priority: i32,
    flags: u32,
    nodes: u32,
}

/// The title reads the list type right after the header.
#[repr(C)]
struct Queue {
    object: Object,
    kind: u32,
    desc: QueueDesc,
    state: Arc<QueueState>,
}

/// What a queue's thread has still to do, in submission order.
#[derive(Default)]
struct QueueState {
    pending: Mutex<Pending>,
    /// A job was queued, done, or its submission waits or goes on.
    changed: Condvar,
}

#[derive(Default)]
struct Pending {
    /// Numbered in order.
    jobs: VecDeque<(u64, Job)>,
    /// The last number given out, and the last job done.
    queued: u64,
    done: u64,
    /// The job running waits for title memory.
    parked: bool,
    worker: bool,
}

enum Job {
    /// Title lists.
    Lists(Vec<usize>),
    /// A fence (referenced while queued) and the value to signal.
    Signal(usize, u64),
    /// A fence (referenced while queued) and the value to wait for.
    Wait(usize, u64),
}

/// Fence signals queued behind waiting submissions.
static SIGNALS: AtomicU64 = AtomicU64::new(0);

thread_local! {
    /// The queue whose thread this is.
    static WORKER: std::cell::RefCell<Option<Arc<QueueState>>> =
        const { std::cell::RefCell::new(None) };
}

/// Title lists in queued submissions (once per submission): reset only
/// once these ran.
static QUEUED: Mutex<Vec<usize>> = Mutex::new(Vec::new());
static RAN: Condvar = Condvar::new();

/// Submissions so far: a wait on a queue's thread goes on waiting while
/// the title still submits.
static SUBMISSIONS: AtomicU64 = AtomicU64::new(0);

pub(crate) fn submissions() -> u64 {
    SUBMISSIONS.load(Ordering::Relaxed)
}

/// The submission running on this queue's thread waits for title memory
/// (`true`) or goes on: whoever submitted it goes on too. Whether this is
/// a queue's thread.
pub(crate) fn park(parked: bool) -> bool {
    WORKER.with_borrow(|worker| {
        let Some(state) = worker else {
            return false;
        };
        state.pending.lock().unwrap_or_else(|e| e.into_inner()).parked = parked;
        state.changed.notify_all();
        true
    })
}

/// Before the title resets `list`: waits until its queued submissions ran
/// (released, it is held until then).
pub(crate) fn settle(list: This) {
    let mut queued = QUEUED.lock().unwrap_or_else(|e| e.into_inner());
    while queued.contains(&(list as usize)) {
        queued = RAN.wait(queued).unwrap_or_else(|e| e.into_inner());
    }
}

impl QueueState {
    /// Starts the queue's thread if it has none; false if it cannot.
    fn start(self: &Arc<Self>, p: &mut Pending) -> bool {
        if p.worker {
            return true;
        }
        let state = self.clone();
        let spawned = std::thread::Builder::new()
            .name("victory-gpu-queue".into())
            .spawn(move || work(state));
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "no queue thread: title waits block the submission");
            return false;
        }
        p.worker = true;
        true
    }

    /// Waits until everything queued ran.
    fn drain(&self) {
        let mut p = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        while p.done < p.queued {
            p = self.changed.wait(p).unwrap_or_else(|e| e.into_inner());
        }
    }
}

/// A queue's thread: runs its jobs in order.
fn work(state: Arc<QueueState>) {
    WORKER.set(Some(state.clone()));
    loop {
        let (n, job) = {
            let mut p = state.pending.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if let Some(job) = p.jobs.pop_front() {
                    break job;
                }
                p = state.changed.wait(p).unwrap_or_else(|e| e.into_inner());
            }
        };
        match job {
            Job::Lists(lists) => {
                let lists: Vec<This> = lists.iter().map(|&l| l as This).collect();
                run(&lists);
                let mut queued = QUEUED.lock().unwrap_or_else(|e| e.into_inner());
                for &l in &lists {
                    if let Some(i) = queued.iter().position(|&q| q == l as usize) {
                        queued.swap_remove(i);
                    }
                }
                drop(queued);
                RAN.notify_all();
                for l in lists {
                    crate::list::list_release(l);
                }
            }
            Job::Signal(fence, value) => {
                crate::fence::signal(fence as This, value);
                crate::fence::release(fence as This);
                SIGNALS.fetch_sub(1, Ordering::Relaxed);
            }
            Job::Wait(fence, value) => {
                park(true);
                let started = std::time::Instant::now();
                while crate::fence::completed(fence as This) < value {
                    if started.elapsed() >= std::time::Duration::from_secs(2) {
                        tracing::warn!(value, "a queue's fence wait timed out; the queue goes on");
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_micros(200));
                }
                crate::fence::release(fence as This);
            }
        }
        let mut p = state.pending.lock().unwrap_or_else(|e| e.into_inner());
        p.done = n;
        p.parked = false;
        state.changed.notify_all();
    }
}

/// Runs title lists on the host GPU and waits for them.
#[cfg(windows)]
fn run(lists: &[This]) {
    let Some(host) = crate::host::host() else {
        return;
    };
    let runs: Vec<This> = lists
        .iter()
        .copied()
        .filter(|&l| crate::list::take_back_slots(l))
        .collect();
    if runs.len() < lists.len() {
        static REFUSED: std::sync::Once = std::sync::Once::new();
        REFUSED.call_once(|| {
            tracing::warn!(
                "a list submitted again after its descriptor slots were taken back is not run"
            )
        });
    }
    let host_lists: Vec<_> = runs
        .iter()
        .filter_map(|&l| crate::list::host_list(l))
        .collect();
    host.execute(&host_lists);
    for &l in &runs {
        crate::list::lend_slots(l);
    }
}

#[cfg(not(windows))]
fn run(_: &[This]) {}
const _: () = assert!(std::mem::offset_of!(Queue, kind) == 0x10);

fn queue<'a>(this: This) -> &'a Queue {
    // SAFETY: called from queue methods only.
    unsafe { &*this.cast::<Queue>() }
}

pub(crate) extern "system" fn create(
    _: This,
    desc: *const QueueDesc,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes a description (null is tolerated).
    let Some(&desc) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    tracing::debug!(?desc, "command queue");
    let queue = Box::new(Queue {
        object: if crate::device::newer() {
            Object::new(&NEWER_FUNCTIONS)
        } else {
            Object::new(&FUNCTIONS)
        },
        kind: desc.kind,
        desc,
        state: Arc::default(),
    });
    object::hand_out(QUEUE, queue, iid, &QUEUE_IIDS, out)
}

/// The console's queue description: an engine or pipe and a queue on it
/// in place of PC's priority and node mask.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct QueueDescX {
    kind: u32,
    flags: u32,
    engine_or_pipe: u32,
    queue: u32,
}

/// Device method `CreateCommandQueueX`. The host picks engines itself, so
/// the queue is created as on PC.
pub(crate) extern "system" fn create_x(
    this: This,
    desc: *const QueueDescX,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes a description (null is tolerated).
    let Some(&x) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    tracing::debug!(desc = ?x, "console command queue");
    let desc = QueueDesc {
        kind: x.kind,
        priority: 0,
        flags: x.flags,
        nodes: 1,
    };
    create(this, &desc, iid, out)
}

extern "system" fn query(this: This, iid: *const Iid, out: *mut This) -> HRESULT {
    object::answer(
        QUEUE,
        iid,
        object::is(iid, &QUEUE_IIDS).then_some(this),
        out,
    )
}
extern "system" fn release(this: This) -> u32 {
    object::release_boxed::<Queue>(this)
}

/// Runs the lists' host lists and waits for them (decision 0007); on the
/// queue's thread if one waits for title memory or the queue still has
/// work, returning once they ran or wait.
extern "system" fn execute(this: This, count: u32, lists: *const This) {
    if lists.is_null() {
        return;
    }
    #[cfg(windows)]
    if crate::host::host().is_some() {
        SUBMISSIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: the title passes `count` lists.
        let lists = unsafe { std::slice::from_raw_parts(lists, count as usize) };
        let state = &queue(this).state;
        let mut p = state.pending.lock().unwrap_or_else(|e| e.into_inner());
        if p.done == p.queued && !lists.iter().any(|&l| crate::list::has_waits(l)) {
            drop(p);
            run(lists);
            return;
        }
        if !state.start(&mut p) {
            drop(p);
            run(lists);
            return;
        }
        // Held until they ran, as the console's queue holds them.
        for &l in lists {
            object::add_ref(l);
        }
        QUEUED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend(lists.iter().map(|&l| l as usize));
        p.queued += 1;
        let n = p.queued;
        p.jobs.push_back((n, Job::Lists(lists.iter().map(|&l| l as usize).collect())));
        state.changed.notify_all();
        // As on the console, the lists' commands before a wait run now.
        while p.done < n && !p.parked {
            p = state.changed.wait(p).unwrap_or_else(|e| e.into_inner());
        }
        return;
    }
    static ONCE: AtomicBool = AtomicBool::new(false);
    if !ONCE.swap(true, Ordering::Relaxed) {
        tracing::warn!(count, "command lists submitted; no host GPU runs them");
    }
}

/// One plane of a `PresentX` call (`D3D12XBOX_PRESENT_PLANE_PARAMETERS`,
/// 0x48 bytes): frame token, resource array, view rects, color space,
/// extended descriptors, and plane flags.
#[repr(C)]
pub(crate) struct Plane {
    token: u64,
    count: u32,
    resources: *const This,
    _rest: [u64; 6],
}
const _: () = assert!(std::mem::size_of::<Plane>() == 0x48);
/// Submission tagged with the frame it belongs to `(count, lists, frame
/// token, flags)`; frames are paced elsewhere, so it runs as a plain one.
extern "system" fn execute_x(this: This, count: u32, lists: *const This, _token: u64, _flags: u32) {
    execute(this, count, lists);
}
/// Signal with sync settings for the top and bottom of the pipe; our
/// submissions are complete when they return, so it signals at once.
extern "system" fn signal2_x(
    this: This,
    fence: This,
    value: u64,
    _bottom: *const u8,
    _top: *const u8,
) -> HRESULT {
    signal(this, fence, value)
}
/// Markers and events only matter to profilers.
extern "system" fn marker(_: This) {}
extern "system" fn signal(this: This, fence: This, value: u64) -> HRESULT {
    if fence.is_null() {
        return E_INVALIDARG;
    }
    let state = &queue(this).state;
    let mut p = state.pending.lock().unwrap_or_else(|e| e.into_inner());
    if p.done < p.queued {
        // After the queue's submissions still to run.
        object::add_ref(fence);
        SIGNALS.fetch_add(1, Ordering::Relaxed);
        p.queued += 1;
        let n = p.queued;
        p.jobs.push_back((n, Job::Signal(fence as usize, value)));
        state.changed.notify_all();
        return S_OK;
    }
    drop(p);
    crate::fence::signal(fence, value)
}
/// Nothing runs on the queue, so waiting for a fence never blocks it;
/// unless a signal is queued behind a waiting submission: then what the
/// title queues here after it runs once the fence reaches the value.
extern "system" fn wait(this: This, fence: This, value: u64) -> HRESULT {
    if fence.is_null() {
        return E_INVALIDARG;
    }
    if SIGNALS.load(Ordering::Relaxed) > 0 && crate::fence::completed(fence) < value {
        let state = &queue(this).state;
        let mut p = state.pending.lock().unwrap_or_else(|e| e.into_inner());
        if state.start(&mut p) {
            object::add_ref(fence);
            p.queued += 1;
            let n = p.queued;
            p.jobs.push_back((n, Job::Wait(fence as usize, value)));
            state.changed.notify_all();
        }
    }
    S_OK
}
/// Ticks per second of GPU timestamps (see [`Written::GpuTimestamp`]).
extern "system" fn timestamp_frequency(_: This, out: *mut u64) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: checked non-null.
    unsafe { out.write(TIMESTAMP_FREQUENCY) };
    S_OK
}
const TIMESTAMP_FREQUENCY: u64 = 100_000_000;

/// What a list's bottom-of-pipe write stores when the list has run.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Written {
    Value(u64),
    /// The GPU clock: [`TIMESTAMP_FREQUENCY`] ticks per second since the
    /// process started (our GPU work is done on the CPU's timeline).
    GpuTimestamp,
    /// The CPU's performance counter.
    SystemTimestamp,
}

impl Written {
    /// Stores the low `bytes` (4 or 8) of the value at `at`.
    pub(crate) fn write(self, at: *mut u8, bytes: usize) {
        let value = match self {
            Written::Value(v) => v,
            Written::GpuTimestamp => {
                static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
                let elapsed = START.get_or_init(std::time::Instant::now).elapsed();
                (elapsed.as_nanos() / (1_000_000_000 / u128::from(TIMESTAMP_FREQUENCY))) as u64
            }
            Written::SystemTimestamp => system_timestamp(),
        };
        // SAFETY: the title names writable GPU memory (unified with ours).
        unsafe {
            if bytes == 4 {
                at.cast::<u32>().write_unaligned(value as u32);
            } else {
                at.cast::<u64>().write_unaligned(value);
            }
        }
    }
}

fn system_timestamp() -> u64 {
    #[cfg(windows)]
    {
        let mut count = 0;
        // SAFETY: a valid out pointer.
        unsafe { windows_sys::Win32::System::Performance::QueryPerformanceCounter(&mut count) };
        count as u64
    }
    #[cfg(not(windows))]
    0
}

/// Frames presented so far.
static FRAMES: AtomicU64 = AtomicU64::new(0);

pub(crate) fn presented() -> u64 {
    FRAMES.load(Ordering::Relaxed)
}

/// Shows the first plane's first resource in our window.
///
/// shortcut: one plane, no scaling or placement, the source expected in
/// the common state.
extern "system" fn present(
    this: This,
    planes: u32,
    plane_parameters: *const Plane,
    _parameters: *const u8,
) -> HRESULT {
    // The frame's submissions still waiting run first.
    if !this.is_null() {
        queue(this).state.drain();
    }
    let frame = FRAMES.fetch_add(1, Ordering::Relaxed) + 1;
    if frame.is_power_of_two() {
        let (draws, dispatches) = crate::list::take_counts();
        #[cfg(windows)]
        let (drawn, dispatched) = {
            static DRAWN: AtomicU64 = AtomicU64::new(0);
            static DISPATCHED: AtomicU64 = AtomicU64::new(0);
            let since = |last: &AtomicU64, now: u64| now - last.swap(now, Ordering::Relaxed);
            (
                since(&DRAWN, crate::draw::drawn()),
                since(&DISPATCHED, crate::draw::dispatched()),
            )
        };
        #[cfg(not(windows))]
        let (drawn, dispatched) = (0, 0);
        tracing::info!(
            frame,
            draws,
            drawn,
            dispatches,
            dispatched,
            "frame presented (since the last line: draws and dispatches, and those run on the host)"
        );
        #[cfg(windows)]
        if frame >= 256 {
            let (hits, misses) = crate::shader_cache::counts();
            tracing::info!(hits, misses, "shaders from the cache and translated so far");
        }
    }
    #[cfg(all(windows, feature = "diagnostics"))]
    if crate::capture::listed(frame) {
        crate::capture::dump_targets(frame);
    }
    #[cfg(windows)]
    if planes > 0
        && !plane_parameters.is_null()
        && let Some(host) = crate::host::host()
    {
        const COPY_SOURCE: u32 = 0x800;
        // SAFETY: the title passes `planes` plane descriptions.
        let slice = unsafe { std::slice::from_raw_parts(plane_parameters, planes.min(8) as usize) };
        let mut resources = Vec::with_capacity(slice.len());
        for plane in slice {
            if plane.count == 0 || plane.resources.is_null() {
                continue;
            }
            // SAFETY: the plane lists `count` resources.
            let resource = unsafe { *plane.resources };
            if let Some(texture) = crate::resource::host_resource(resource) {
                let state = crate::resource::host_transition(resource, COPY_SOURCE)
                    .map_or(COPY_SOURCE, |(_, before)| before);
                resources.push((resource, texture, state));
            }
        }
        #[cfg(feature = "diagnostics")]
        let fallback_1440p = crate::capture::take_1440p_target();
        if !resources.is_empty() {
            #[cfg_attr(not(feature = "diagnostics"), allow(unused_mut))]
            let mut sources: Vec<(&_, u32)> = resources
                .iter()
                .map(|(_, tex, state)| (*tex, *state))
                .collect();
            #[cfg(feature = "diagnostics")]
            if let Some(ref copy) = fallback_1440p {
                let state = crate::host::legal_state(
                    &copy.texture,
                    copy.state.load(Ordering::Relaxed),
                );
                static ONCE: std::sync::Once = std::sync::Once::new();
                ONCE.call_once(|| {
                    // SAFETY: a live host texture.
                    let d = unsafe { copy.texture.GetDesc() };
                    tracing::info!(
                        width = d.Width,
                        height = d.Height,
                        format = d.Format.0,
                        "VICTORY_PRESENT_1440P: presenting 2560x1440 world target as plane 0"
                    );
                });
                sources[0] = (&copy.texture, state);
            }
            let shown = host.present(&sources, frame);
            for &(resource, _, state) in &resources {
                crate::resource::host_transition(resource, state);
            }
            #[cfg(feature = "diagnostics")]
            blinks(frame, resources[0].0, resources[0].2, shown);
            #[cfg(not(feature = "diagnostics"))]
            let _ = shown;
        }
    }
    #[cfg(not(windows))]
    let _ = (planes, plane_parameters);
    S_OK
}
/// Diagnostics: logs a presented frame that is black after one that was
/// not, with what was presented and how many draws ran since.
#[cfg(all(windows, feature = "diagnostics"))]
fn blinks(frame: u64, resource: This, state: u32, shown: Option<bool>) {
    static LAST: std::sync::Mutex<(bool, u64, u64)> = std::sync::Mutex::new((false, 0, 0));
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    let drawn = crate::draw::drawn();
    let black = shown.unwrap_or(false);
    let draws = crate::draw::take_frame();
    if black && !last.0 {
        last.2 += 1;
        tracing::warn!(
            frame,
            resource = ?resource,
            state = format_args!("{state:#x}"),
            draws = drawn - last.1,
            copied = shown.is_some(),
            black_frames = last.2,
            levels = ?crate::capture::recent_levels(),
            "a black frame after a frame that was not"
        );
        for d in draws {
            tracing::warn!("black frame {frame}: {d}");
        }
        AFTER.store(frame + 6, Ordering::Relaxed);
    }
    // Six frames on: the levels around it (a fade or a single drop).
    static AFTER: AtomicU64 = AtomicU64::new(0);
    if AFTER.load(Ordering::Relaxed) == frame {
        tracing::warn!(frame, levels = ?crate::capture::recent_levels(), "levels after the black frame");
    }
    *last = (black, drawn, last.2);
}

/// A run of destination pages: how they are mapped (0 pool pages from
/// `start` on, 1 pool page `start` each, 2 unmapped, 3 left as they are)
/// and how many.
#[repr(C)]
pub(crate) struct PageRange {
    kind: u32,
    start: u32,
    count: u32,
}

/// The ranges mapped from a destination address on.
#[repr(C)]
pub(crate) struct PageBatch {
    destination: u64,
    count: u32,
    ranges: *const PageRange,
}

/// Bytes per mapped page.
const PAGE: u64 = 64 << 10;

/// `CopyPageMappingsBatchX(count, batches, pool, pool pages, flags)`: maps
/// pages of a pool (memory the title mapped physical pages into) at the
/// batches' addresses. Grand Theft Auto VI streams textures into pool pages
/// and maps them into its tiled heap this way (title+0xf2c470, ranges of
/// one constant page or of unmapped pages). Done at once: every earlier
/// submission has run.
extern "system" fn copy_page_mappings(
    this: This,
    count: u32,
    batches: *const PageBatch,
    pool: u64,
    pool_pages: u32,
    _flags: u32,
) -> HRESULT {
    // Queued submissions use the mappings they were submitted with.
    if !this.is_null() {
        queue(this).state.drain();
    }
    let Some(map) = crate::resource::page_mapper() else {
        return E_INVALIDARG;
    };
    if batches.is_null() && count > 0 {
        return E_INVALIDARG;
    }
    let batches = (count > 0).then(|| {
        // SAFETY: the title passes `count` batches.
        unsafe { std::slice::from_raw_parts(batches, count as usize) }
    });
    let mut ok = true;
    for b in batches.into_iter().flatten() {
        if b.ranges.is_null() {
            continue;
        }
        // SAFETY: each batch holds `count` ranges.
        let ranges = unsafe { std::slice::from_raw_parts(b.ranges, b.count as usize) };
        let mut at = b.destination;
        for r in ranges {
            let len = u64::from(r.count) * PAGE;
            let inside = |pages: u32| r.start.checked_add(pages).is_some_and(|e| e <= pool_pages);
            let source = pool + u64::from(r.start) * PAGE;
            ok &= match r.kind {
                0 if inside(r.count) => map(at, source, len),
                1 if inside(1) => (0..u64::from(r.count)).all(|k| map(at + k * PAGE, source, PAGE)),
                2 => map(at, 0, len),
                3 => true,
                _ => false,
            };
            at += len;
        }
    }
    if !ok {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::warn!(pool = format_args!("{pool:#x}"), "a page mapping failed"));
    }
    S_OK
}

extern "system" fn desc(this: This, out: *mut QueueDesc) {
    if !out.is_null() {
        // SAFETY: checked non-null; the title passes room for a description.
        unsafe { out.write(queue(this).desc) };
    }
}

/// The queue methods by their slot in older titles' queues (the 2020-era
/// layout): IUnknown 3, object 4, device child 1, queue 26 (12 as on PC,
/// then console ones).
const METHODS: [(usize, *const ()); 15] = [
    (0, query as *const ()),
    (2, release as *const ()),
    (7, object::parent_device as *const ()), // 0x38
    (10, execute as *const ()),              // 0x50
    (11, marker as *const ()),               // 0x58 set marker
    (12, marker as *const ()),               // 0x60 begin event
    (13, marker as *const ()),               // 0x68 end event
    (14, signal as *const ()),               // 0x70
    (15, wait as *const ()),                 // 0x78
    (16, timestamp_frequency as *const ()),  // 0x80
    (26, desc as *const ()),                 // 0xd0
    (29, copy_page_mappings as *const ()),   // 0xe8 page mapping batch
    (30, present as *const ()),              // 0xf0
    (33, execute_x as *const ()),            // 0x108
    (32, signal2_x as *const ()),            // 0x100
];

/// Newer titles' queues have two more methods before Signal, which moved
/// from 0x70 to 0x80: every slot from 0x70 on is two later (taken here as
/// starting exactly at 0x70, the methods before it kept in place).
/// Inferred from Grand Theft Auto VI's calls: Signal 0x80 `(fence,
/// value)`, timestamp frequency 0x90 (title+0x24841a6), suspend and resume
/// 0xb8/0xc0, PresentX 0x100, the frame-token submission 0x118 `(count,
/// lists, token, flags)` (title+0x24821dd).
const fn newer_slot(slot: usize) -> usize {
    if slot >= 14 { slot + 2 } else { slot }
}

const fn queue_table(newer: bool) -> [*const (); 36] {
    let mut f = object::d3d12_object(object::missing_table::<QUEUE, 36>());
    let mut i = 0;
    while i < METHODS.len() {
        let (slot, method) = METHODS[i];
        f[if newer { newer_slot(slot) } else { slot }] = method;
        i += 1;
    }
    f
}

static FUNCTIONS: Table<36> = Table(queue_table(false));
static NEWER_FUNCTIONS: Table<36> = Table(queue_table(true));

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::slot;

    /// Where a queue's signal is: newer layouts have it two slots later.
    /// Another test may switch layouts while this one runs, so the queue's
    /// own table decides.
    fn signal_slot(q: This) -> usize {
        // SAFETY: one of our queues; its first field is the table.
        let table = unsafe { *q.cast::<*const *const ()>() };
        if table == NEWER_FUNCTIONS.0.as_ptr() {
            0x80
        } else {
            0x70
        }
    }

    #[test]
    fn queue_signals_complete_at_once() {
        let null = std::ptr::null();
        let desc = QueueDesc {
            kind: 0,
            priority: 0,
            flags: 0,
            nodes: 0,
        };
        let mut q = null;
        assert_eq!(create(null, &desc, &QUEUE_IIDS[0], &mut q), S_OK);
        // SAFETY: the title reads the type at +0x10.
        assert_eq!(unsafe { q.cast::<u8>().add(0x10).cast::<u32>().read() }, 0);
        let mut fence = null;
        let fence_iid = &crate::fence::FENCE_IIDS[0];
        assert_eq!(
            crate::fence::create(null, 0, 0, fence_iid, &mut fence),
            S_OK
        );
        let signal = slot::<extern "system" fn(This, This, u64) -> HRESULT>(q, signal_slot(q));
        assert_eq!(signal(q, fence, 7), S_OK);
        assert_eq!(
            slot::<extern "system" fn(This) -> u64>(fence, 0x40)(fence),
            7
        );
        assert_eq!(slot::<extern "system" fn(This) -> u32>(q, 0x10)(q), 0);
    }

    /// Grand Theft Auto VI's compute queue: `{COMPUTE, 0, pipe 1, queue 0}`.
    #[test]
    fn console_queues_are_created_as_on_pc() {
        let null = std::ptr::null();
        let desc = QueueDescX {
            kind: 2,
            flags: 0,
            engine_or_pipe: 1,
            queue: 0,
        };
        let mut q = null;
        assert_eq!(create_x(null, &desc, &QUEUE_IIDS[0], &mut q), S_OK);
        assert_eq!(queue(q).desc.kind, 2);
        let none = std::ptr::null();
        assert_eq!(create_x(null, none, &QUEUE_IIDS[0], &mut q), E_INVALIDARG);
    }

    #[test]
    fn newer_queues_have_their_methods_two_slots_later() {
        let (old, new) = (queue_table(false), queue_table(true));
        assert_eq!(old[10], new[10]); // submission stays at 0x50
        assert_eq!(old[14], new[16]); // signal 0x70 → 0x80
        assert_eq!(old[30], new[32]); // present 0xf0 → 0x100
        assert_eq!(new[35], execute_x as *const ()); // 0x118
    }

    #[test]
    fn timestamps_and_values_are_written() {
        let mut at = [0u64; 2];
        Written::Value(0x1_0000_0002).write(at.as_mut_ptr().cast(), 4);
        assert_eq!(at[0], 2);
        Written::GpuTimestamp.write(at[1..].as_mut_ptr().cast(), 8);
        let first = at[1];
        std::thread::sleep(std::time::Duration::from_millis(2));
        Written::GpuTimestamp.write(at[1..].as_mut_ptr().cast(), 8);
        assert!(at[1] > first);
    }

    #[cfg(windows)]
    fn make_queue_and_list() -> (This, This, This) {
        let null = std::ptr::null();
        let desc = QueueDesc {
            kind: 0,
            priority: 0,
            flags: 0,
            nodes: 0,
        };
        let mut q = null;
        assert_eq!(create(null, &desc, &QUEUE_IIDS[0], &mut q), S_OK);
        let mut alloc = null;
        let alloc_iid = guid(
            0x6102dee4,
            0xaf59,
            0x4b09,
            [0xb9, 0x99, 0xb4, 0x4d, 0x73, 0xf0, 0x9b, 0x24],
        );
        assert_eq!(
            crate::list::create_allocator(null, 0, &alloc_iid, &mut alloc),
            S_OK
        );
        let mut list = null;
        let list_iid = guid(
            0x5b160d0f,
            0xac1b,
            0x4185,
            [0x8b, 0xa8, 0xb3, 0xae, 0x42, 0xa5, 0xa4, 0x55],
        );
        assert_eq!(
            crate::list::create_list(null, 0, 0, alloc, null, &list_iid, &mut list),
            S_OK
        );
        (q, alloc, list)
    }

    #[cfg(windows)]
    fn list_slot_offset(list: This, old_offset: usize) -> usize {
        if crate::list::has_newer_table(list) && old_offset >= 59 * 8 {
            old_offset + 16
        } else {
            old_offset
        }
    }

    #[cfg(windows)]
    fn wait_fence_value(fence: This, want: u64) {
        let completed = slot::<extern "system" fn(This) -> u64>(fence, 0x40);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while completed(fence) < want {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for fence value {want} (got {})",
                completed(fence)
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    #[cfg(windows)]
    #[test]
    fn command_list_memory_wait_orders_producer_and_consumer_both_ways() {
        for consumer_first in [false, true] {
            let (prod_q, prod_alloc, prod_list) = make_queue_and_list();
            let (cons_q, cons_alloc, cons_list) = make_queue_and_list();
            let mut fence = std::ptr::null();
            assert_eq!(
                crate::fence::create(
                    std::ptr::null(),
                    0,
                    0,
                    &crate::fence::FENCE_IIDS[0],
                    &mut fence
                ),
                S_OK
            );

            let payload = Box::new(AtomicU64::new(0));
            let counter = Box::new(AtomicU64::new(0));
            let result = Box::new(AtomicU64::new(0));

            let write64 = slot::<extern "system" fn(This, u64, u64, u32, u32)>(
                prod_list,
                list_slot_offset(prod_list, 0x230),
            );
            let close_p = slot::<extern "system" fn(This) -> HRESULT>(prod_list, 0x48);
            write64(prod_list, (&raw const *payload) as u64, 0x5a55a11c, 0, 0);
            write64(prod_list, (&raw const *counter) as u64, 2, 0, 0);
            assert_eq!(close_p(prod_list), S_OK);

            let wait64 = slot::<extern "system" fn(This, u64, u32, u64, u32)>(
                cons_list,
                list_slot_offset(cons_list, 0x240),
            );
            let copy_mem = slot::<extern "system" fn(This, u64, u64, u64, u32)>(
                cons_list,
                list_slot_offset(cons_list, 0x1e0),
            );
            let close_c = slot::<extern "system" fn(This) -> HRESULT>(cons_list, 0x48);
            wait64(cons_list, (&raw const *counter) as u64, 7, 1, 0);
            copy_mem(
                cons_list,
                (&raw const *result) as u64,
                (&raw const *payload) as u64,
                8,
                0,
            );
            assert_eq!(close_c(cons_list), S_OK);

            let exec_p = slot::<extern "system" fn(This, u32, *const This)>(prod_q, 0x50);
            let exec_c = slot::<extern "system" fn(This, u32, *const This)>(cons_q, 0x50);
            let sig_c =
                slot::<extern "system" fn(This, This, u64) -> HRESULT>(cons_q, signal_slot(cons_q));
            let completed = slot::<extern "system" fn(This) -> u64>(fence, 0x40);

            if consumer_first {
                exec_c(cons_q, 1, &cons_list);
                assert_eq!(sig_c(cons_q, fence, 1), S_OK);
                assert_eq!(completed(fence), 0);
                assert_eq!(result.load(Ordering::Acquire), 0);
                exec_p(prod_q, 1, &prod_list);
                wait_fence_value(fence, 1);
            } else {
                exec_p(prod_q, 1, &prod_list);
                exec_c(cons_q, 1, &cons_list);
                assert_eq!(sig_c(cons_q, fence, 1), S_OK);
                assert_eq!(completed(fence), 1);
            }

            assert_eq!(counter.load(Ordering::Acquire), 2);
            assert_eq!(payload.load(Ordering::Acquire), 0x5a55a11c);
            assert_eq!(
                result.load(Ordering::Acquire),
                0x5a55a11c,
                "consumer_first={consumer_first}"
            );

            for o in [
                prod_list, prod_alloc, prod_q, cons_list, cons_alloc, cons_q, fence,
            ] {
                slot::<extern "system" fn(This) -> u32>(o, 0x10)(o);
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn mid_list_memory_wait_preserves_pre_wait_work_and_releases_post_wait_tail() {
        let (prod_q, prod_alloc, prod_list) = make_queue_and_list();
        let (cons_q, cons_alloc, cons_list) = make_queue_and_list();
        let mut fence = std::ptr::null();
        assert_eq!(
            crate::fence::create(
                std::ptr::null(),
                0,
                0,
                &crate::fence::FENCE_IIDS[0],
                &mut fence
            ),
            S_OK
        );

        let pre_src = Box::new(AtomicU64::new(0xfeed_face_cafe_babe));
        let pre_dst = Box::new(AtomicU64::new(0));
        let pre_marker = Box::new(AtomicU64::new(0));
        let gate = Box::new(AtomicU64::new(0));
        let post_src = Box::new(AtomicU64::new(0));
        let post_dst = Box::new(AtomicU64::new(0));
        let post_marker = Box::new(AtomicU64::new(0));

        let write64_c = slot::<extern "system" fn(This, u64, u64, u32, u32)>(
            cons_list,
            list_slot_offset(cons_list, 0x230),
        );
        let wait64_c = slot::<extern "system" fn(This, u64, u32, u64, u32)>(
            cons_list,
            list_slot_offset(cons_list, 0x240),
        );
        let copy_c = slot::<extern "system" fn(This, u64, u64, u64, u32)>(
            cons_list,
            list_slot_offset(cons_list, 0x1e0),
        );
        let close_c = slot::<extern "system" fn(This) -> HRESULT>(cons_list, 0x48);

        // Pre-wait work.
        copy_c(
            cons_list,
            (&raw const *pre_dst) as u64,
            (&raw const *pre_src) as u64,
            8,
            0,
        );
        write64_c(
            cons_list,
            (&raw const *pre_marker) as u64,
            0x1111_2222_3333_4444,
            0,
            0,
        );
        // Full 64-bit threshold: low 32 bits alone must not satisfy it.
        wait64_c(
            cons_list,
            (&raw const *gate) as u64,
            7,
            0x0000_0001_0000_0001,
            1,
        );
        // Post-wait work.
        copy_c(
            cons_list,
            (&raw const *post_dst) as u64,
            (&raw const *post_src) as u64,
            8,
            0,
        );
        write64_c(
            cons_list,
            (&raw const *post_marker) as u64,
            0xaaaa_bbbb_cccc_dddd,
            0,
            0,
        );
        assert_eq!(close_c(cons_list), S_OK);

        let write64_p = slot::<extern "system" fn(This, u64, u64, u32, u32)>(
            prod_list,
            list_slot_offset(prod_list, 0x230),
        );
        let close_p = slot::<extern "system" fn(This) -> HRESULT>(prod_list, 0x48);
        write64_p(
            prod_list,
            (&raw const *post_src) as u64,
            0x5a55_a11c_dead_beef,
            0,
            0,
        );
        write64_p(
            prod_list,
            (&raw const *gate) as u64,
            0x0000_0001_0000_0001,
            0,
            0,
        );
        assert_eq!(close_p(prod_list), S_OK);

        let exec_c = slot::<extern "system" fn(This, u32, *const This)>(cons_q, 0x50);
        let exec_p = slot::<extern "system" fn(This, u32, *const This)>(prod_q, 0x50);
        let sig_c =
            slot::<extern "system" fn(This, This, u64) -> HRESULT>(cons_q, signal_slot(cons_q));
        let completed = slot::<extern "system" fn(This) -> u64>(fence, 0x40);

        // Submit consumer first: pre-wait work completes immediately; post-wait stays pending.
        exec_c(cons_q, 1, &cons_list);
        assert_eq!(sig_c(cons_q, fence, 1), S_OK);
        assert_eq!(pre_dst.load(Ordering::Acquire), 0xfeed_face_cafe_babe);
        assert_eq!(pre_marker.load(Ordering::Acquire), 0x1111_2222_3333_4444);
        assert_eq!(post_dst.load(Ordering::Acquire), 0);
        assert_eq!(post_marker.load(Ordering::Acquire), 0);
        assert_eq!(completed(fence), 0);

        // Low-32-bit write only must not release a 64-bit wait.
        gate.store(1, Ordering::Release);
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert_eq!(post_dst.load(Ordering::Acquire), 0);
        assert_eq!(completed(fence), 0);

        // Producer writes full 64-bit payload and gate, releasing post-wait work and fence.
        exec_p(prod_q, 1, &prod_list);
        wait_fence_value(fence, 1);
        assert_eq!(post_dst.load(Ordering::Acquire), 0x5a55_a11c_dead_beef);
        assert_eq!(post_marker.load(Ordering::Acquire), 0xaaaa_bbbb_cccc_dddd);

        for o in [
            prod_list, prod_alloc, prod_q, cons_list, cons_alloc, cons_q, fence,
        ] {
            slot::<extern "system" fn(This) -> u32>(o, 0x10)(o);
        }
    }
}
