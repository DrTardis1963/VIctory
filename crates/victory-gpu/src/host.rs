//! The host GPU: one D3D12 device and direct queue (decision 0007), and a
//! window to present on.
//!
//! shortcut: every submission is waited for, so CPU and GPU never overlap;
//! overlap them with per-submission fences once more than clears run.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{
    D3D_FEATURE_LEVEL_12_0, D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, ID3DBlob,
};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{Interface, PCSTR, PCWSTR, s, w};

/// Resource states without the console's own bits (compression hints from
/// bit 25 up), which the host rejects.
pub(crate) fn pc_state(state: u32) -> u32 {
    state & 0x01ff_ffff
}

/// A console state as a host texture state: PC bits only, and none of the
/// buffer-only ones the console allows on textures (vertex, constant and
/// index buffer, stream out, indirect arguments, as in "generic read").
pub(crate) fn texture_state(state: u32) -> u32 {
    const BUFFER_ONLY: u32 = 0x1 | 0x2 | 0x100 | 0x200;
    pc_state(state) & !BUFFER_ONLY
}

/// Render target views live only until their command is recorded, so a
/// ring of host descriptors is enough.
const RTV_RING: u32 = 1024;
type Prelude = (ID3D12CommandAllocator, ID3D12GraphicsCommandList);

pub(crate) struct Host {
    pub(crate) device: ID3D12Device,
    pub(crate) depth_bounds_supported: bool,
    queue: ID3D12CommandQueue,
    fence: ID3D12Fence,
    /// The last value signalled on `fence`, behind the submission lock.
    submitted: Mutex<u64>,
    indirect_prelude: OnceLock<windows::core::Result<Prelude>>,
    event: HANDLE,
    rtvs: ID3D12DescriptorHeap,
    rtv_size: u32,
    /// A ring of depth views, used as `rtvs`.
    dsvs: ID3D12DescriptorHeap,
    dsv_size: u32,
    next_dsv: AtomicU32,
    /// With `VICTORY_D3D12_DEBUG` set (diagnostics builds): the debug
    /// layer's messages.
    messages: Option<ID3D12InfoQueue>,
    next_rtv: AtomicU32,
    presenter: Mutex<Option<Presenter>>,
}

// SAFETY: D3D12 devices, queues, fences and descriptor heaps are
// free-threaded; the event handle is only waited on under `submitted`.
unsafe impl Send for Host {}
// SAFETY: as above.
unsafe impl Sync for Host {}

/// A host resource waiting to be dropped after in-flight GPU work finishes.
struct Retired {
    frame: u64,
    #[allow(dead_code)]
    resource: ID3D12Resource,
}

// SAFETY: D3D12 resources are free-threaded and may be released on any thread.
unsafe impl Send for Retired {}

/// Host resources retired on worker threads (for example when a descriptor
/// slot is overwritten or a title resource is released while a submission is
/// executing on the GPU). Kept alive for several presented frames because a
/// single title frame spans many submissions and worker threads may record
/// lists ahead of presentation.
static RETIRED: Mutex<Vec<Retired>> = Mutex::new(Vec::new());

/// Holds `resource` alive until several full frames have completed on the GPU.
pub(crate) fn retire(resource: ID3D12Resource) {
    let frame = crate::queue::presented();
    RETIRED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(Retired { frame, resource });
}

/// Ages are in frames presented to the host (not the title's 60 Hz clock, which
/// ticks many times per frame while the host runs at 1-2 fps).
/// Takes retired resources old enough to drop once the current submission
/// finishes on the GPU.
fn take_retired() -> Vec<Retired> {
    const KEEP_FRAMES: u64 = 4;
    let now = crate::queue::presented();
    let mut guard = RETIRED.lock().unwrap_or_else(|e| e.into_inner());
    let mut ready = Vec::new();
    let mut i = 0;
    while i < guard.len() {
        if now.saturating_sub(guard[i].frame) >= KEEP_FRAMES {
            ready.push(guard.swap_remove(i));
        } else {
            i += 1;
        }
    }
    ready
}

/// Time spent in submissions since the last frame-rate line, in
/// microseconds: preparing them on the CPU (title memory copies, texture
/// detiling), then waiting for the GPU.
static SUBMISSION_US: [std::sync::atomic::AtomicU64; 2] =
    [std::sync::atomic::AtomicU64::new(0), std::sync::atomic::AtomicU64::new(0)];

/// After submissions: shader-written ranges compared and copied back into
/// title memory (microseconds, bytes that differed), since the last
/// frame-rate line.
static COPY_BACK: [std::sync::atomic::AtomicU64; 2] =
    [std::sync::atomic::AtomicU64::new(0), std::sync::atomic::AtomicU64::new(0)];

/// Of the preparing: title memory copies (microseconds, bytes) and texture
/// refreshes (microseconds, bytes hashed), since the last frame-rate line.
pub(crate) static PREPARING: [std::sync::atomic::AtomicU64; 4] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

/// What the runtime does when the host GPU fails for good: report it like a
/// crash and end the run. The console has no such state, and carrying on
/// draws nothing at seconds per frame.
static ON_FAILURE: OnceLock<fn(&str) -> !> = OnceLock::new();

/// Sets what a host GPU failure does (the runtime's crash report).
pub fn on_failure(report: fn(&str) -> !) {
    let _ = ON_FAILURE.set(report);
}

/// Ends the run on a host GPU failure: what failed, the removal reason and
/// the debug layer's first error, if any (`VICTORY_D3D12_DEBUG=1` in a
/// diagnostics build names the command).
pub(crate) fn fail(what: std::fmt::Arguments) -> ! {
    let mut text = what.to_string();
    if let Some(host) = host() {
        // SAFETY: a plain query.
        if let Err(e) = unsafe { host.device.GetDeviceRemovedReason() } {
            text.push_str(&format!("; device removed: {e}"));
        }
        match host.log_messages() {
            Some(first) => text.push_str(&format!("; debug layer: {first}")),
            None if host.messages.is_none() => text.push_str(
                "; a diagnostics build with VICTORY_D3D12_DEBUG=1 names the command",
            ),
            None => {}
        }
    }
    #[cfg(test)]
    if validate_d3d12_tests() {
        panic!("{text}");
    }
    tracing::error!("{text}");
    match ON_FAILURE.get() {
        Some(report) => report(&text),
        None => std::process::abort(),
    }
}

#[cfg(test)]
fn validate_d3d12_tests() -> bool {
    std::env::var_os("VICTORY_TEST_D3D12_VALIDATE").is_some()
}

/// The host GPU, created on first use; `None` without one (a run then
/// behaves as before: nothing executes).
pub(crate) fn host() -> Option<&'static Host> {
    static HOST: OnceLock<Option<Host>> = OnceLock::new();
    HOST.get_or_init(|| match create() {
        Ok(host) => Some(host),
        Err(e) => {
            #[cfg(test)]
            assert!(
                !validate_d3d12_tests(),
                "could not create the validation device: {e}"
            );
            tracing::warn!(error = %e, "no host D3D12 device; command lists will not run");
            None
        }
    })
    .as_ref()
}

fn create() -> windows::core::Result<Host> {
    // SAFETY: plain D3D12 object creation with valid descriptions.
    unsafe {
        // The debug layer only in builds with diagnostics.
        let debug =
            cfg!(feature = "diagnostics") && std::env::var_os("VICTORY_D3D12_DEBUG").is_some();
        if debug {
            let mut layer: Option<ID3D12Debug> = None;
            match D3D12GetDebugInterface(&mut layer) {
                Ok(()) => layer.iter().for_each(|l| l.EnableDebugLayer()),
                Err(e) => tracing::warn!(error = %e, "no D3D12 debug layer"),
            }
            // GPU-based validation (`VICTORY_D3D12_GBV`): out-of-range
            // descriptor heap indices and the like become messages naming
            // the command instead of page faults. Production diagnostics
            // skip state tracking for directly indexed textures. Strict
            // GPU tests keep it enabled.
            if std::env::var_os("VICTORY_D3D12_GBV").is_some()
                && let Some(l) = &layer
            {
                if let Ok(d1) = l.cast::<ID3D12Debug1>() {
                    d1.SetEnableGPUBasedValidation(true);
                    #[cfg(test)]
                    if validate_d3d12_tests() {
                        eprintln!("GPU-based validation enabled with resource state tracking");
                    }
                } else {
                    #[cfg(test)]
                    assert!(
                        !validate_d3d12_tests(),
                        "GPU-based validation is unavailable"
                    );
                }
                #[cfg(test)]
                let disable_state_tracking = !validate_d3d12_tests();
                #[cfg(not(test))]
                let disable_state_tracking = true;
                if disable_state_tracking && let Ok(d2) = l.cast::<ID3D12Debug2>() {
                    d2.SetGPUBasedValidationFlags(
                        D3D12_GPU_BASED_VALIDATION_FLAGS_DISABLE_STATE_TRACKING,
                    );
                }
                tracing::info!("GPU-based validation on");
            }
        }
        // Diagnostics: device-removed data (the command that faulted, the
        // faulting address), read when the device is removed.
        if dred_on() {
            let mut dred: Option<ID3D12DeviceRemovedExtendedDataSettings> = None;
            if D3D12GetDebugInterface(&mut dred).is_ok()
                && let Some(dred) = dred
            {
                dred.SetAutoBreadcrumbsEnablement(D3D12_DRED_ENABLEMENT_FORCED_ON);
                dred.SetPageFaultEnablement(D3D12_DRED_ENABLEMENT_FORCED_ON);
            }
        }
        let mut device: Option<ID3D12Device> = None;
        // Diagnostics: the software adapter, on which a shader's wild access
        // is an access violation in this process (a crash report with the
        // address and instruction) instead of a GPU fault and driver reset.
        if cfg!(feature = "diagnostics") && std::env::var_os("VICTORY_D3D12_WARP").is_some() {
            let adapter = warp_adapter()?;
            D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device)?;
            tracing::info!("host device on the software adapter (WARP)");
        } else {
            #[cfg(test)]
            let adapter = if std::env::var_os("VICTORY_TEST_BUFFER_COPY_NVIDIA").is_some() {
                let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))?;
                let nvidia = (0..)
                    .map_while(|i| factory.EnumAdapters1(i).ok())
                    .find(|a| a.GetDesc1().is_ok_and(|d| d.VendorId == 0x10de))
                    .ok_or_else(windows::core::Error::empty)?;
                Some(nvidia.cast::<windows::core::IUnknown>()?)
            } else {
                None
            };
            #[cfg(not(test))]
            let adapter: Option<windows::core::IUnknown> = None;
            D3D12CreateDevice(adapter.as_ref(), D3D_FEATURE_LEVEL_12_0, &mut device)?;
        }
        let device = device.ok_or_else(windows::core::Error::empty)?;
        let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS2::default();
        let depth_bounds_supported = device
            .CheckFeatureSupport(
                D3D12_FEATURE_D3D12_OPTIONS2,
                (&raw mut options).cast(),
                size_of_val(&options) as u32,
            )
            .is_ok()
            && options.DepthBoundsTestSupported.as_bool();
        let messages = debug
            .then(|| device.cast::<ID3D12InfoQueue>().ok())
            .flatten();
        #[cfg(test)]
        if validate_d3d12_tests() {
            assert!(messages.is_some(), "D3D12 debug layer is not active");
            let luid = device.GetAdapterLuid();
            let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))?;
            let adapter: IDXGIAdapter1 = factory.EnumAdapterByLuid(luid)?;
            let desc = adapter.GetDesc1()?;
            let name = String::from_utf16_lossy(&desc.Description);
            eprintln!(
                "D3D12 debug layer active on {}",
                name.trim_end_matches('\0')
            );
        }
        let queue: ID3D12CommandQueue = device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
            Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
            ..Default::default()
        })?;
        let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE)?;
        let event = CreateEventW(None, false, false, None)?;
        let rtvs: ID3D12DescriptorHeap =
            device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
                NumDescriptors: RTV_RING,
                ..Default::default()
            })?;
        let rtv_size = device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_RTV);
        let dsvs: ID3D12DescriptorHeap =
            device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_DSV,
                NumDescriptors: RTV_RING,
                ..Default::default()
            })?;
        let dsv_size = device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_DSV);
        tracing::info!("host D3D12 device ready");
        Ok(Host {
            device,
            depth_bounds_supported,
            queue,
            fence,
            submitted: Mutex::new(0),
            indirect_prelude: OnceLock::new(),
            event,
            rtvs,
            rtv_size,
            dsvs,
            dsv_size,
            next_dsv: AtomicU32::new(0),
            messages,
            next_rtv: AtomicU32::new(0),
            presenter: Mutex::new(None),
        })
    }
}

/// DXGI may return hardware for `EnumWarpAdapter` (notably under Proton).
/// Reject it before creating a device in diagnostics' software-only mode.
fn warp_adapter() -> windows::core::Result<IDXGIAdapter> {
    // SAFETY: plain DXGI adapter queries.
    unsafe {
        let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))?;
        let adapter: IDXGIAdapter = factory.EnumWarpAdapter()?;
        let desc = adapter.cast::<IDXGIAdapter1>()?.GetDesc1()?;
        if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 == 0 {
            tracing::warn!(
                vendor = format_args!("{:#x}", desc.VendorId),
                flags = format_args!("{:#x}", desc.Flags),
                "DXGI did not identify the WARP adapter as software; refusing device"
            );
            return Err(windows::core::Error::from_hresult(
                windows::Win32::Foundation::E_FAIL,
            ));
        }
        Ok(adapter)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn sparse_copies_preserve_islands_and_observe_remaps() {
        use windows_sys::Win32::System::Memory::*;
        const N: usize = 5 << 16;
        // SAFETY: we reserve a private region, commit selected pages inside it,
        // and only directly access the pages while they are writable.
        unsafe {
            let base = VirtualAlloc(std::ptr::null(), N, MEM_RESERVE, PAGE_NOACCESS).cast::<u8>();
            assert!(!base.is_null());
            let mut expected = vec![0; N];
            for (offset, value) in [(0, 9), (2 << 16, 37), (N - 4096, 85)] {
                let at = base.add(offset);
                assert_eq!(VirtualAlloc(at.cast(), 4096, MEM_COMMIT, PAGE_READWRITE), at.cast());
                at.write_bytes(value, 4096);
                expected[offset..offset + 4096].fill(value);
            }
            for remap in [false, true] {
                if remap {
                    let mut old = 0;
                    assert_ne!(VirtualProtect(base.cast(), 4096, PAGE_NOACCESS, &mut old), 0);
                    expected[..4096].fill(0);
                    let at = base.add(1 << 16);
                    assert_eq!(VirtualAlloc(at.cast(), 4096, MEM_COMMIT, PAGE_READWRITE), at.cast());
                    at.write_bytes(0xc7, 4096);
                    expected[1 << 16..(1 << 16) + 4096].fill(0xc7);
                }
                let mut out = vec![0xee; N];
                super::copy_mapped(out.as_mut_ptr().add(27), base.add(27), N - 54, false);
                assert_eq!(out[27..N - 27], expected[27..N - 27]);
                assert!(out[..27].iter().chain(&out[N - 27..]).all(|&v| v == 0xee));
            }
            assert_ne!(VirtualFree(base.cast(), 0, MEM_RELEASE), 0);
        }
    }

    use super::*;

    /// A stop deep in a long run of commands between two barriers names
    /// the commands around the stop, not only the first ones after the
    /// barrier.
    #[test]
    fn in_flight_names_the_commands_at_the_stop() {
        let mut history = vec![D3D12_AUTO_BREADCRUMB_OP_RESOURCEBARRIER];
        let mut markers = Vec::new();
        for n in 0..100u32 {
            markers.push((history.len() as u32, format!("dispatch {n}")));
            history.push(D3D12_AUTO_BREADCRUMB_OP_SETMARKER);
            history.push(D3D12_AUTO_BREADCRUMB_OP_DISPATCH);
        }
        history.push(D3D12_AUTO_BREADCRUMB_OP_RESOURCEBARRIER);
        // Dispatch 80 is command 1 + 2 * 80 + 1.
        let text = in_flight(&history, &markers, 162);
        assert!(
            text.starts_with(", commands 1..201 between its barriers:"),
            "{text}"
        );
        assert!(text.contains("command 162 (op 6): dispatch 80"), "{text}");
        assert!(text.contains("command 160 (op 6): dispatch 79"), "{text}");
        assert!(!text.contains("dispatch 0;"), "{text}");
    }

    #[test]
    fn warp_adapter_accepts_only_software() {
        // This probes DXGI without creating a D3D12 device or running a title.
        // SAFETY: plain DXGI adapter queries.
        let raw = unsafe {
            let Ok(factory) = CreateDXGIFactory2::<IDXGIFactory4>(DXGI_CREATE_FACTORY_FLAGS(0))
            else {
                return;
            };
            factory.EnumWarpAdapter::<IDXGIAdapter>()
        };
        let Ok(raw) = raw else { return };
        // SAFETY: EnumWarpAdapter returns a DXGI adapter, which implements
        // IDXGIAdapter1; `raw` owns that interface.
        let desc = unsafe { raw.cast::<IDXGIAdapter1>().unwrap().GetDesc1().unwrap() };
        eprintln!(
            "EnumWarpAdapter: vendor={:#x}, flags={:#x}",
            desc.VendorId, desc.Flags
        );
        assert_eq!(
            warp_adapter().is_ok(),
            desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0,
        );
    }
}

/// A host command list and its allocator, always used together.
pub(crate) struct HostList {
    allocator: ID3D12CommandAllocator,
    pub(crate) list: ID3D12GraphicsCommandList,
    pub(crate) depth_bounds: Option<ID3D12GraphicsCommandList1>,
    /// Native lists recorded after a title buffer-copy boundary.
    continuations: Mutex<Vec<Continuation>>,
    has_buffer_copies: AtomicBool,
    /// Raw host lists may record native buffer work without deferred steps.
    may_access_buffers: AtomicBool,
    /// Work for submission time, in recorded order.
    deferred: Mutex<Deferred>,
    /// Upload memory for data the list's draws make (expanded vertices),
    /// reused after a reset.
    scratch: Mutex<Scratch>,
    /// Memory shaders write indirect arguments to, reused after a reset.
    arguments: Mutex<Arguments>,
    /// Host texture states the list assumes, by host copy: at its start and
    /// at its end. Lists are recorded on many threads and run in submission
    /// order, so the transitions from a texture's real state are made at
    /// submission, in `prelude`. Holding the copies keeps every texture the
    /// list names alive until it is reset, even if the title releases the
    /// resource first.
    states: Mutex<States>,
    /// The list that runs those transitions just before this one.
    prelude: Prelude,
    /// The pipelines the list binds, for reports of slow submissions.
    pipelines: Mutex<std::collections::BTreeSet<String>>,
    /// The work its indirect executions have reserved (`indirect`): a
    /// shader-writable word, zeroed when the list is submitted.
    budget: Arc<OnceLock<Option<(ID3D12Resource, usize)>>>,
    /// The texts of the markers `note_op` set, in order (for device-removed
    /// data without marker texts), with the host GPU range a copy reads.
    #[cfg(feature = "diagnostics")]
    notes: Mutex<Vec<Note>>,
    /// With `VICTORY_GPU_TIMING`: a GPU timestamp before each noted command
    /// and at the end, and where they are read back.
    #[cfg(feature = "diagnostics")]
    stamps: OnceLock<Option<(ID3D12QueryHeap, ID3D12Resource)>>,
}

pub(crate) fn warn_depth_bounds() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| tracing::warn!("the host GPU has no depth bounds test; depth bounds are ignored"));
}

/// Timestamps per list (diagnostics); commands past them are not timed.
#[cfg(feature = "diagnostics")]
const STAMPS: u32 = 4096;

/// A marker's text and the host range its command reads (diagnostics).
#[cfg(feature = "diagnostics")]
type Note = (String, Option<(u64, u64)>);

type States = std::collections::HashMap<
    usize,
    (u32, u32, std::sync::Arc<crate::resource::HostTexture>),
>;

/// Whether device-removed data is on (`VICTORY_D3D12_DRED`, diagnostics
/// builds).
pub(crate) fn dred_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        cfg!(feature = "diagnostics") && std::env::var_os("VICTORY_D3D12_DRED").is_some()
    })
}

/// Whether each list of a submission runs and is timed on its own, slow
/// ones logged with their commands (`VICTORY_GPU_TIMING`, diagnostics
/// builds; the commands are noted with `VICTORY_D3D12_DRED` on).
#[cfg(feature = "diagnostics")]
fn timing() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_GPU_TIMING").is_some())
}

/// Names a host object for the device-removed report, which names the
/// allocations a page fault hit (only with device-removed data on).
pub(crate) fn name(object: &impl Interface, text: impl FnOnce() -> String) {
    if dred_on()
        && let Ok(object) = object.cast::<ID3D12Object>()
    {
        // SAFETY: a plain call on a live object.
        let _ = unsafe { object.SetName(&windows::core::HSTRING::from(text())) };
    }
}

/// Upload chunks handed out front to back: the chunks, the one in use and
/// the bytes used in it.
#[derive(Default)]
struct Scratch {
    chunks: Vec<(ID3D12Resource, usize, u64)>,
    current: usize,
    used: u64,
}

// SAFETY: the chunk pointers are persistent mappings of upload buffers.
unsafe impl Send for Scratch {}

/// Chunks of [`ARGUMENT_CHUNK`] bytes, the first `filled` in use, the last
/// of them up to `used`.
#[derive(Default)]
struct Arguments {
    chunks: Vec<ID3D12Resource>,
    filled: usize,
    used: u64,
}

// SAFETY: D3D12 resources are free-threaded; used under the list's lock.
unsafe impl Send for Arguments {}

const ARGUMENT_CHUNK: u64 = 1 << 16;

const SCRATCH_CHUNK: u64 = 4 << 20;

/// One piece of submission-time work, in recorded order.
#[derive(Clone)]
enum Continuation {
    Copy(Arc<BufferCopy>),
    Commands(Arc<HostList>),
    Wait(MemoryWait),
}

/// A title wait in a list (`Wait32BitValueX`, `Wait64BitValueX`): until
/// `(memory & mask) <comparison> value`, `comparison` a
/// `D3D12_COMPARISON_FUNC` (1 never .. 8 always). The list's commands
/// before it run first, then it is waited for on the CPU, without the
/// submission lock (another queue's submission may be what releases it),
/// up to [`WAIT_TIMEOUT`].
#[derive(Clone, Copy, Debug)]
pub(crate) struct MemoryWait {
    pub(crate) address: usize,
    /// 64 bits, else 32.
    pub(crate) wide: bool,
    pub(crate) comparison: u32,
    pub(crate) value: u64,
    pub(crate) mask: u64,
    /// The list's bottom-of-pipe writes recorded before it: done before it
    /// is waited for.
    writes_before: usize,
}

/// How long a title wait is waited for before the list goes on (logged).
const WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// How long a title wait is waited for while no other submission runs and
/// its memory stays as it was: only a title thread could release it then.
const SHORT_WAIT: std::time::Duration = std::time::Duration::from_millis(50);

/// Threads in [`Host::execute`], those waiting for the submission lock
/// included.
static SUBMITTING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// `lhs <comparison> rhs` for a `D3D12_COMPARISON_FUNC`; `None` if unknown.
pub(crate) fn compare(comparison: u32, lhs: u64, rhs: u64) -> Option<bool> {
    Some(match comparison {
        1 => false,
        2 => lhs < rhs,
        3 => lhs == rhs,
        4 => lhs <= rhs,
        5 => lhs > rhs,
        6 => lhs != rhs,
        7 => lhs >= rhs,
        8 => true,
        _ => return None,
    })
}

impl MemoryWait {
    pub(crate) fn new(address: usize, wide: bool, comparison: u32, value: u64, mask: u64) -> Self {
        MemoryWait {
            address,
            wide,
            comparison,
            value,
            mask,
            writes_before: 0,
        }
    }

    pub(crate) fn len(&self) -> usize {
        if self.wide { 8 } else { 4 }
    }

    /// The memory now, if it is mapped.
    fn read(&self) -> Option<u64> {
        if !crate::texture::committed(self.address as *const u8, self.len()) {
            return None;
        }
        // SAFETY: committed, aligned (checked when recorded); other
        // threads write it, as another queue would.
        Some(unsafe {
            if self.wide {
                (self.address as *const u64).read_volatile()
            } else {
                u64::from((self.address as *const u32).read_volatile())
            }
        })
    }

    /// Whether the memory satisfies the wait now (memory unmapped since it
    /// was recorded: it does, as nothing can write it).
    pub(crate) fn holds(&self) -> bool {
        self.read().is_none_or(|now| {
            compare(self.comparison, now & self.mask, self.value).unwrap_or(true)
        })
    }

    /// Waits until it holds, up to [`WAIT_TIMEOUT`], or [`SHORT_WAIT`]
    /// while no other submission runs and the memory does not change; else
    /// how long it waited.
    fn wait(&self) -> Result<(), std::time::Duration> {
        let started = std::time::Instant::now();
        let first = self.read();
        while !self.holds() {
            let waited = started.elapsed();
            let others = SUBMITTING.load(Ordering::Relaxed) > 1;
            if waited >= WAIT_TIMEOUT || (waited >= SHORT_WAIT && !others && self.read() == first)
            {
                return Err(waited);
            }
            std::thread::sleep(if waited < std::time::Duration::from_millis(10) {
                std::time::Duration::from_micros(100)
            } else {
                std::time::Duration::from_millis(1)
            });
        }
        Ok(())
    }

    fn written_by(&self, writes: &[(usize, crate::queue::Written, usize)]) -> bool {
        writes
            .iter()
            .any(|&(at, _, bytes)| overlaps((at, bytes), (self.address, self.len())))
    }
}

struct BufferCopy {
    to: usize,
    from: usize,
    len: usize,
    buffers: [crate::resource::BufferLease; 2],
    native: OnceLock<NativeCopy>,
}

struct NativeCopy {
    lists: Vec<HostList>,
    access: Option<CopyAccess>,
}

#[derive(Clone, Copy)]
struct CopyAccess {
    source: usize,
    target: usize,
    written: (usize, usize),
}

impl CopyAccess {
    fn independent_of(self, previous: Self) -> bool {
        self.source != previous.target
            && self.target != previous.source
            && (self.target != previous.target || !overlaps(self.written, previous.written))
    }
}

#[derive(Clone, Copy)]
enum QueueSegment {
    Commands,
    Copy { starts_group: bool },
}

impl BufferCopy {
    fn native(&self, host: &Host) -> &NativeCopy {
        self.native.get_or_init(|| {
            let new_list = || {
                host.new_list().unwrap_or_else(|e| {
                    fail(format_args!(
                        "a buffer copy command list did not open ({e})"
                    ))
                })
            };
            let (to, from, len) = (self.to, self.from, self.len);
            let copy = new_list();
            let mut lists = Vec::new();
            let mut access = None;
            let source = crate::shadow::find(from as u64, len as u64);
            let target = crate::shadow::find(to as u64, len as u64);
            if let (Some(source), Some(target)) = (source, target) {
                let (mut buffer, mut offset) = (source.buffer.clone(), source.offset(from as u64));
                if buffer == target.buffer {
                    // Legacy barriers cannot make one buffer COPY_SOURCE
                    // and COPY_DEST at once. Decay between the scratch
                    // transfer and the copy back to the same buffer.
                    let (scratch, _) = host.shared_buffer(len as u64).unwrap_or_else(|| {
                        fail(format_args!("no scratch for aliased buffer copy"))
                    });
                    let first = new_list();
                    source.refresh(&first, from as u64, len as u64);
                    first.keep(&scratch);
                    // SAFETY: both live resources cover the named ranges.
                    unsafe {
                        first
                            .list
                            .CopyBufferRegion(&scratch, 0, &buffer, offset, len as u64)
                    };
                    first.close_native();
                    lists.push(first);
                    buffer = scratch;
                    offset = 0;
                    copy.keep(&buffer);
                } else {
                    source.refresh(&copy, from as u64, len as u64);
                    access = Some(CopyAccess {
                        source: buffer.as_raw() as usize,
                        target: target.buffer.as_raw() as usize,
                        written: (target.offset(to as u64) as usize, len),
                    });
                }
                target.refresh(&copy, to as u64, len as u64);
                copy.defer_gpu_write(&target, to, len);
                // SAFETY: the live, distinct buffers cover both ranges. The
                // copy executes on its own and both buffers can promote.
                unsafe {
                    copy.list.CopyBufferRegion(
                        &target.buffer,
                        target.offset(to as u64),
                        &buffer,
                        offset,
                        len as u64,
                    )
                };
            } else {
                // A mapped fallback must wait for a pending GPU producer.
                copy.defer_copy(to as *mut u8, from as *const u8, len);
            }
            copy.close_native();
            lists.push(copy);
            NativeCopy { lists, access }
        })
    }
}

enum Step {
    /// A copy into title memory (destination, source, bytes): the title's
    /// buffer copies, which later refreshes must see.
    Copy(usize, usize, usize),
    /// A copy of title memory into the list's own upload memory.
    Fill(usize, usize, usize),
    /// Brings a range (address, bytes) of a buffer's host copy up to date:
    /// once per submission, whichever lists ask, until a `Copy` or `Run`
    /// intervenes.
    Refresh(std::sync::Arc<crate::shadow::Shadow>, usize, usize),
    /// Work that may write title memory.
    Run(Box<dyn Fn() + Send>),
    /// Work that writes host memory only.
    Host(Option<(usize, usize)>, Box<dyn Fn() + Send>),
    /// Shaders write a range (address, bytes) of a buffer's host copy.
    GpuWrite(std::sync::Arc<crate::shadow::Shadow>, usize, usize),
    /// Brings a texture's staging buffer up to date: once per submission,
    /// whichever lists sample it, until a `Copy` or `Run` intervenes.
    /// The host copy is copied from it in the list's prelude if it lacks
    /// its bytes.
    Texture(
        std::sync::Arc<crate::texture::Staging>,
        std::sync::Arc<crate::resource::HostTexture>,
    ),
}

/// What a list does on the CPU when it is submitted: copies between host
/// memory regions (title buffers, upload buffers and buffer copies), and
/// the upload buffers its GPU commands read, kept until the list is reset.
#[derive(Default)]
struct Deferred {
    /// Destination, source, bytes.
    copies: Vec<Step>,
    uploads: Vec<ID3D12Resource>,
    /// Host objects the list's commands name (pipelines, root signatures,
    /// descriptor heaps): the title may release its objects before it
    /// submits the list.
    kept: Vec<ID3D12Object>,
    /// Whole buffers whose indirect uses need enhanced barriers.
    indirect_buffers: std::collections::HashMap<usize, ID3D12Resource>,
    /// Buffer ranges refreshed since the last title copy (host copy,
    /// address, bytes): refreshing them again would find the same bytes.
    refreshed: std::collections::HashSet<(usize, usize, usize)>,
    /// View heaps the list's draws index directly (`ResourceDescriptorHeap[]`),
    /// and whether any of their shaders may write memory: their slots are
    /// brought up to date once per submission, after every other step.
    bindless: Vec<(std::sync::Arc<crate::descriptor::host::HostHeap>, bool)>,
    /// Incremented whenever a deferred step may write title memory (`Copy` or
    /// `Run`), invalidating per-list bindless preparation caches.
    copy_epoch: u64,
    /// Textures whose contents the list brings up to date.
    textures: std::collections::HashSet<usize>,
    /// Bottom-of-pipe writes, done once the list has run: address, what,
    /// bytes.
    writes: Vec<(usize, crate::queue::Written, usize)>,
}

impl Host {
    /// Whether the host runs mesh shaders at shader model 6.6 (translated
    /// mesh shaders need both); logged once.
    pub(crate) fn mesh_shaders(&self) -> bool {
        static MESH: OnceLock<bool> = OnceLock::new();
        *MESH.get_or_init(|| {
            let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS7::default();
            let mut model = D3D12_FEATURE_DATA_SHADER_MODEL {
                HighestShaderModel: D3D_SHADER_MODEL_6_6,
            };
            // SAFETY: each query writes its own structure of the given size.
            let ok = unsafe {
                self.device
                    .CheckFeatureSupport(
                        D3D12_FEATURE_D3D12_OPTIONS7,
                        (&raw mut options).cast(),
                        size_of_val(&options) as u32,
                    )
                    .is_ok()
                    && self
                        .device
                        .CheckFeatureSupport(
                            D3D12_FEATURE_SHADER_MODEL,
                            (&raw mut model).cast(),
                            size_of_val(&model) as u32,
                        )
                        .is_ok()
            };
            let mesh = ok
                && options.MeshShaderTier != D3D12_MESH_SHADER_TIER_NOT_SUPPORTED
                && model.HighestShaderModel.0 >= D3D_SHADER_MODEL_6_6.0;
            tracing::info!(mesh, "host mesh shader support");
            mesh
        })
    }

    /// The wave sizes the host runs (D3D12 options 1), which translated
    /// compute shaders that declare the console's wave size must fit;
    /// logged once. Unknown (the query fails): any.
    pub(crate) fn target(&self) -> victory_shader::emit::Target {
        static TARGET: OnceLock<victory_shader::emit::Target> = OnceLock::new();
        *TARGET.get_or_init(|| {
            let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS1::default();
            // SAFETY: the query writes the structure of the given size.
            let ok = unsafe {
                self.device
                    .CheckFeatureSupport(
                        D3D12_FEATURE_D3D12_OPTIONS1,
                        (&raw mut options).cast(),
                        size_of_val(&options) as u32,
                    )
                    .is_ok()
            };
            let target = if ok && options.WaveOps.as_bool() && options.WaveLaneCountMin > 0 {
                victory_shader::emit::Target {
                    lanes_min: options.WaveLaneCountMin,
                    lanes_max: options.WaveLaneCountMax,
                }
            } else {
                victory_shader::emit::Target::ANY
            };
            tracing::info!(
                lanes_min = target.lanes_min,
                lanes_max = target.lanes_max,
                "host wave sizes"
            );
            if !target.runs(64) {
                tracing::warn!(
                    "the host has no 64-lane waves: console wave64 compute programs that read                      other lanes do not translate, and their dispatches are dropped"
                );
            }
            target
        })
    }

    /// A new list, open for recording.
    pub(crate) fn new_list(&self) -> windows::core::Result<HostList> {
        // SAFETY: plain object creation.
        unsafe {
            let allocator: ID3D12CommandAllocator = self
                .device
                .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
            let list: ID3D12GraphicsCommandList = self.device.CreateCommandList(
                0,
                D3D12_COMMAND_LIST_TYPE_DIRECT,
                &allocator,
                None,
            )?;
            Ok(HostList {
                allocator,
                depth_bounds: self.depth_bounds_supported.then(|| list.cast()).transpose()?,
                list,
                continuations: Mutex::default(),
                has_buffer_copies: AtomicBool::new(false),
                may_access_buffers: AtomicBool::new(true),
                deferred: Mutex::default(),
                scratch: Mutex::default(),
                arguments: Mutex::default(),
                states: Mutex::default(),
                prelude: self.new_prelude()?,
                pipelines: Mutex::default(),
                budget: Arc::default(),
                #[cfg(feature = "diagnostics")]
                notes: Mutex::default(),
                #[cfg(feature = "diagnostics")]
                stamps: OnceLock::new(),
            })
        }
    }

    fn new_prelude(&self) -> windows::core::Result<Prelude> {
        // SAFETY: a direct allocator and a list created against it.
        unsafe {
            let allocator = self
                .device
                .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
            let list: ID3D12GraphicsCommandList = self.device.CreateCommandList(
                0,
                D3D12_COMMAND_LIST_TYPE_DIRECT,
                &allocator,
                None,
            )?;
            list.Close()?;
            Ok((allocator, list))
        }
    }

    /// Selects enhanced barriers for all indirect buffers before any list
    /// in this submission can implicitly promote them to a legacy state.
    /// The caller waits for a prior prelude still running before this
    /// re-records it.
    fn indirect_prelude(
        &self,
        runs: &[&HostList],
    ) -> windows::core::Result<Option<ID3D12CommandList>> {
        let mut buffers = std::collections::HashMap::new();
        for list in runs {
            buffers.extend(
                list.deferred()
                    .indirect_buffers
                    .iter()
                    .map(|(&key, buffer)| (key, buffer.clone())),
            );
        }
        if buffers.is_empty() {
            return Ok(None);
        }
        let (allocator, list) = self
            .indirect_prelude
            .get_or_init(|| self.new_prelude())
            .as_ref()
            .map_err(Clone::clone)?;
        // SAFETY: the preceding submission finished, and buffers live
        // through this submission. A new submission starts them in COMMON.
        unsafe {
            allocator.Reset()?;
            list.Reset(allocator, None)?;
            crate::indirect::buffer_barriers(
                &list.cast::<ID3D12GraphicsCommandList7>()?,
                buffers.values(),
                (D3D12_BARRIER_SYNC_NONE, D3D12_BARRIER_ACCESS_NO_ACCESS),
                (D3D12_BARRIER_SYNC_ALL, D3D12_BARRIER_ACCESS_COMMON),
            );
            list.Close()?;
        }
        list.cast().map(Some)
    }

    pub(crate) fn new_title_list(&self) -> windows::core::Result<HostList> {
        let list = self.new_list()?;
        list.may_access_buffers.store(false, Ordering::Relaxed);
        Ok(list)
    }

    /// Runs closed lists and waits for them.
    pub(crate) fn execute(&self, runs: &[&HostList]) {
        SUBMITTING.fetch_add(1, Ordering::Relaxed);
        struct Done;
        impl Drop for Done {
            fn drop(&mut self) {
                SUBMITTING.fetch_sub(1, Ordering::Relaxed);
            }
        }
        let _done = Done;
        // One submission at a time from preparation to copy-back: snapshots
        // and the ranges shaders write are per host copy, not per submission.
        let mut submitted = self.submitted.lock().unwrap_or_else(|e| e.into_inner());
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::Submissions, 1);

        for list in runs {
            if let Some(Some((_, data))) = list.budget.get() {
                // SAFETY: this list's budget is mapped, and under the
                // submission lock the GPU finished any prior use. Its
                // continuations share the word, so it is zeroed once here.
                unsafe { (*data as *mut u32).write_volatile(0) };
            }
        }
        let started = std::time::Instant::now();
        let continuations: Vec<_> = runs
            .iter()
            .map(|list| {
                if list.has_buffer_copies.load(Ordering::Relaxed) {
                    list.continuations().clone()
                } else {
                    Vec::new()
                }
            })
            .collect();
        let has_copies = continuations.iter().any(|parts| !parts.is_empty());
        // Per run, how many of its bottom-of-pipe writes are done (before a
        // wait).
        let mut written = vec![0; runs.len()];
        let mut ordered = Vec::new();
        // Title waits: before which entry of `ordered`, in which run.
        let mut waits: Vec<(usize, usize, usize, MemoryWait)> = Vec::new();
        let mut gpu_work = false;
        for (run, (&list, parts)) in runs.iter().zip(&continuations).enumerate() {
            ordered.push((list, QueueSegment::Commands));
            gpu_work |= has_copies && list.may_access_buffers();
            let mut copy_group: Vec<CopyAccess> = Vec::new();
            let mut previous_copy: Option<&BufferCopy> = None;
            for (p, part) in parts.iter().enumerate() {
                match part {
                    Continuation::Commands(list) => {
                        ordered.push((list, QueueSegment::Commands));
                        gpu_work |= list.may_access_buffers();
                        copy_group.clear();
                        previous_copy = None;
                    }
                    Continuation::Copy(copy) if gpu_work => {
                        if previous_copy.is_some_and(|previous| {
                            previous.to == copy.to
                                && previous.from == copy.from
                                && previous.len == copy.len
                                && previous.buffers == copy.buffers
                                && previous.native(self).access.is_some()
                        }) {
                            continue;
                        }
                        let copy = copy.as_ref();
                        let native = copy.native(self);
                        let starts_group = copy_group.is_empty()
                            || native.access.is_none_or(|access| {
                                copy_group
                                    .iter()
                                    .any(|&earlier| !access.independent_of(earlier))
                            });
                        if starts_group {
                            copy_group.clear();
                        }
                        copy_group.extend(native.access);
                        ordered.extend(native.lists.iter().enumerate().map(|(i, list)| {
                            let segment = QueueSegment::Copy {
                                starts_group: starts_group || i != 0,
                            };
                            (list, segment)
                        }));
                        previous_copy = Some(copy);
                    }
                    Continuation::Copy(copy) => {
                        // No preceding command can have touched title
                        // buffers. Retain the cheap mapped copy in this prefix.
                        run_mapped_copy(copy.to, copy.from, copy.len, true);
                    }
                    Continuation::Wait(wait) => {
                        waits.push((ordered.len(), run, p, *wait));
                        // Copies after it read what is there once it holds.
                        gpu_work = true;
                        copy_group.clear();
                        previous_copy = None;
                    }
                }
            }
        }
        SUBMISSION_US[0].fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
        if !has_copies {
            self.execute_batch(&ordered, &mut submitted);
        } else {
            let mut start = 0;
            let mut pending = Vec::new();
            let mut waits = waits.into_iter().peekable();
            for i in 0..=ordered.len() {
                while let Some((_, run, part, wait)) = waits.next_if(|&(at, ..)| at == i) {
                    // The bottom-of-pipe writes recorded before the wait.
                    let before: Vec<(usize, usize)> = (0..=run)
                        .map(|r| {
                            let n = runs[r].deferred().writes.len();
                            let end = if r == run { wait.writes_before.min(n) } else { n };
                            (written[r].min(end), end)
                        })
                        .collect();
                    let unwritten = before.iter().enumerate().any(|(r, &(from, end))| {
                        wait.written_by(&runs[r].deferred().writes[from..end])
                    });
                    // Holding already and released by nothing before it.
                    if wait.holds() && !unwritten {
                        continue;
                    }
                    // What releases it runs after it in this submission:
                    // blocking cannot help (on the console another queue
                    // runs it meanwhile).
                    if !unwritten
                        && let Some(what) =
                            released_later(runs, &continuations, &ordered[i..], run, part, &wait)
                    {
                        wait_not_waited(&wait, what);
                        continue;
                    }
                    if i != start {
                        self.execute_batch(&ordered[start..i], &mut submitted);
                        pending.clear();
                        start = i;
                    }
                    for (r, &(from, end)) in before.iter().enumerate() {
                        for &(at, what, bytes) in &runs[r].deferred().writes[from..end] {
                            perform_write(at, what, bytes);
                        }
                        written[r] = written[r].max(end);
                    }
                    if !wait.holds() {
                        drop(submitted);
                        let waited = wait.wait();
                        submitted = self.submitted.lock().unwrap_or_else(|e| e.into_inner());
                        if let Err(waited) = waited {
                            wait_timed_out(&wait, waited);
                        }
                    }
                }
                let Some(&(list, _)) = ordered.get(i) else {
                    break;
                };
                if i != start
                    && (list.needs_title_bytes(&pending)
                        || ordered[start..i]
                            .iter()
                            .any(|(other, _)| std::ptr::eq(*other, list)))
                {
                    self.execute_batch(&ordered[start..i], &mut submitted);
                    pending.clear();
                    start = i;
                }
                list.add_gpu_writes(&mut pending);
            }
            if start < ordered.len() {
                self.execute_batch(&ordered[start..], &mut submitted);
            }
        }
        for (r, list) in runs.iter().enumerate() {
            let deferred = list.deferred();
            for &(at, what, bytes) in &deferred.writes[written[r].min(deferred.writes.len())..] {
                perform_write(at, what, bytes);
            }
        }
        let error = self.log_messages();
        #[cfg(test)]
        if validate_d3d12_tests() {
            assert!(error.is_none(), "D3D12 debug layer reported {error:?}");
        }
        let _ = error;
    }

    fn execute_batch(&self, ordered: &[(&HostList, QueueSegment)], submitted: &mut u64) {
        let started = std::time::Instant::now();
        let runs: Vec<_> = ordered.iter().map(|&(list, _)| list).collect();
        let mut lists: Vec<Option<ID3D12CommandList>> = Vec::with_capacity(2 * runs.len());
        let mut groups = vec![(0, 0)];
        #[cfg(feature = "diagnostics")]
        let timed = timing();
        #[cfg(not(feature = "diagnostics"))]
        let timed = false;
        let mut batch = Batch::default();
        let mut previous_copy = false;
        for (i, &(list, segment)) in ordered.iter().enumerate() {
            list.run_deferred(&mut batch);
            // Legacy buffer states decay at ExecuteCommandLists boundaries.
            // Keep dependent transfers and surrounding shader work in
            // separate executions. Independent transfers can run together.
            let starts_group = match segment {
                QueueSegment::Commands => previous_copy,
                QueueSegment::Copy { starts_group } => starts_group || !previous_copy,
            };
            if i != 0 && (starts_group || timed) {
                groups.push((lists.len(), i));
            }
            previous_copy = matches!(segment, QueueSegment::Copy { .. });
            if let Some(prelude) = list.prelude() {
                lists.push(Some(prelude));
            }
            lists.push(list.list.cast().ok());
        }
        batch.prepare_bindless();
        let writes = batch.snapshot_writes();
        let retired = take_retired();
        let prepared = std::time::Instant::now();
        // An ExecuteCommandLists not waited for yet whose prelude list the
        // next one would re-record.
        let mut prelude_running = false;
        for (group, &(first, i)) in groups.iter().enumerate() {
            let (end, next_run) = groups
                .get(group + 1)
                .map_or((lists.len(), runs.len()), |&(end, next)| (end, next));
            // Buffer state decays at each ExecuteCommandLists boundary, so
            // each one registers its own lists' indirect buffers.
            let mut prefixed = Vec::new();
            let group_runs = &runs[i..next_run];
            if prelude_running
                && group_runs
                    .iter()
                    .any(|list| !list.deferred().indirect_buffers.is_empty())
            {
                *submitted += 1;
                self.wait_for_submission(*submitted);
                prelude_running = false;
            }
            let group_lists = match self.indirect_prelude(group_runs) {
                Ok(Some(prelude)) => {
                    prelude_running = true;
                    prefixed.push(Some(prelude));
                    prefixed.extend_from_slice(&lists[first..end]);
                    prefixed.as_slice()
                }
                Ok(None) => &lists[first..end],
                Err(e) => fail(format_args!("indirect buffer prelude did not record ({e})")),
            };
            let started = std::time::Instant::now();
            // SAFETY: closed lists and a live direct queue.
            unsafe { self.queue.ExecuteCommandLists(group_lists) };
            if timed || group + 1 == groups.len() {
                *submitted += 1;
                self.wait_for_submission(*submitted);
                prelude_running = false;
            }
            #[cfg(feature = "diagnostics")]
            if timed && groups.len() > 1 && started.elapsed() > std::time::Duration::from_millis(50)
            {
                // SAFETY: a plain query.
                let frequency = unsafe { self.queue.GetTimestampFrequency() }.unwrap_or(1);
                let commands = runs[i].notes.lock().unwrap_or_else(|e| e.into_inner()).len();
                tracing::warn!(ms = started.elapsed().as_millis(), commands, "slow host list");
                for (ms, op) in runs[i].slowest(frequency, 6) {
                    tracing::warn!("slow list command, {ms:.1} ms: {op}");
                }
            }
            let _ = (i, started);
        }
        let gpu = prepared.elapsed();
        SUBMISSION_US[0].fetch_add((prepared - started).as_micros() as u64, Ordering::Relaxed);
        SUBMISSION_US[1].fetch_add(gpu.as_micros() as u64, Ordering::Relaxed);
        // The host resets a device whose submission runs for about 2 s.
        if gpu > std::time::Duration::from_millis(250) {
            let pipelines: std::collections::BTreeSet<String> = runs
                .iter()
                .flat_map(|l| l.pipelines.lock().unwrap_or_else(|e| e.into_inner()).clone())
                .collect();
            tracing::warn!(ms = gpu.as_millis(), lists = runs.len(), ?pipelines, "slow host submission");
        }
        // SAFETY: a plain query.
        if unsafe { self.device.GetDeviceRemovedReason() }.is_err() {
            fail(format_args!(
                "host GPU device removed after a submission of {} lists{}{}",
                runs.len(),
                self.removal_data(&runs),
                last_notes(&runs)
            ));
        }
        drop(retired);
        // Copy back merged ranges before a dependent CPU phase or the
        // bottom-of-pipe writes that tell the title the GPU is done.
        let copying = std::time::Instant::now();
        COPY_BACK[1].fetch_add(writes.copy_back() as u64, Ordering::Relaxed);
        COPY_BACK[0].fetch_add(copying.elapsed().as_micros() as u64, Ordering::Relaxed);
    }

    fn wait_for_submission(&self, submitted: u64) {
        // SAFETY: the fence and event are ours.
        unsafe {
            let waited = self
                .queue
                .Signal(&self.fence, submitted)
                .and_then(|()| self.fence.SetEventOnCompletion(submitted, self.event));
            match waited {
                Ok(()) => {
                    WaitForSingleObject(self.event, INFINITE);
                }
                Err(e) => fail(format_args!("host submission not waited for ({e})")),
            }
        }
    }

    /// With `VICTORY_D3D12_DRED`: the faulting address and the allocations
    /// it lies in (by the names we give them) or the title memory copy just
    /// below it; for each list that did not finish, the command it stopped
    /// at and what the draws and dispatches in flight there run (or, in
    /// lists that never started, the pipelines they bind).
    fn removal_data(&self, runs: &[&HostList]) -> String {
        let Ok(dred) = self.device.cast::<ID3D12DeviceRemovedExtendedData1>() else {
            return String::new();
        };
        let mut text = String::new();
        let mut fault_va = None;
        // SAFETY: the outputs are the runtime's, valid while the device
        // lives; nodes are walked as linked, histories have their counts.
        unsafe {
            if let Ok(fault) = dred.GetPageFaultAllocationOutput1()
                && fault.PageFaultVA != 0
            {
                let va = fault.PageFaultVA;
                fault_va = Some(va);
                text.push_str(&format!("; page fault at {va:#x}"));
                for (what, head) in [
                    ("inside", fault.pHeadExistingAllocationNode),
                    ("inside freed", fault.pHeadRecentFreedAllocationNode),
                ] {
                    let mut names = Vec::new();
                    let mut node = head;
                    while let Some(n) = node.as_ref()
                        && names.len() < 8
                    {
                        let name = if n.ObjectNameW.is_null() {
                            "unnamed".to_owned()
                        } else {
                            n.ObjectNameW.to_string().unwrap_or_default()
                        };
                        names.push(format!("{name} (type {})", n.AllocationType.0));
                        node = n.pNext;
                    }
                    if !names.is_empty() {
                        text.push_str(&format!("; {what} {names:?}"));
                    }
                }
                text.push_str(&crate::shadow::near(va));
                text.push_str(&crate::descriptor::host::near_heaps(va));
            }
            // The plain output: the extended one (with marker texts) came
            // back without stopped lists on this host.
            let crumbs = match dred.GetAutoBreadcrumbsOutput() {
                Ok(crumbs) => crumbs,
                Err(e) => {
                    text.push_str(&format!("; no breadcrumbs ({e})"));
                    return text;
                }
            };
            // Every list's progress (done/count), in the runtime's order.
            let mut progress = Vec::new();
            let mut node = crumbs.pHeadAutoBreadcrumbNode;
            while let Some(p) = node.as_ref() {
                let done = p.pLastBreadcrumbValue.as_ref().copied().unwrap_or(0);
                let ours = p.pCommandList.as_ref().map(|l| l.as_raw());
                let index = runs.iter().position(|r| Some(r.list.as_raw()) == ours);
                progress.push(format!("{done}/{} {index:?}", p.BreadcrumbCount));
                node = p.pNext;
            }
            text.push_str(&format!("; breadcrumbs (done/count, list) {progress:?}"));
            let mut plain = crumbs.pHeadAutoBreadcrumbNode;
            while let Some(p) = plain.as_ref() {
                let done = p.pLastBreadcrumbValue.as_ref().copied().unwrap_or(0);
                if done < p.BreadcrumbCount && !p.pCommandHistory.is_null() {
                    let history =
                        std::slice::from_raw_parts(p.pCommandHistory, p.BreadcrumbCount as usize);
                    text.push_str(&format!(
                        "; a list stopped at command {done} of {} (op {})",
                        p.BreadcrumbCount, history[done as usize].0
                    ));
                    let list = p.pCommandList.as_ref().map(|l| l.as_raw());
                    let run = runs.iter().find(|r| Some(r.list.as_raw()) == list);
                    // The k-th marker command is the k-th note.
                    #[cfg(feature = "diagnostics")]
                    let markers: Vec<(u32, String)> = run.map_or_else(Vec::new, |r| {
                        let notes = r.notes.lock().unwrap_or_else(|e| e.into_inner());
                        history
                            .iter()
                            .enumerate()
                            .filter(|(_, op)| **op == D3D12_AUTO_BREADCRUMB_OP_SETMARKER)
                            .zip(notes.iter())
                            .map(|((at, _), (note, range))| {
                                let place = fault_va.zip(*range).map(|(va, r)| fault_in(va, r));
                                (at as u32, format!("{note}{}", place.unwrap_or_default()))
                            })
                            .collect()
                    });
                    #[cfg(not(feature = "diagnostics"))]
                    let markers: Vec<(u32, String)> = Vec::new();
                    #[cfg(not(feature = "diagnostics"))]
                    let _ = fault_va;
                    if done > 0 {
                        text.push_str(&in_flight(history, &markers, done as usize));
                    } else if let Some(r) = run {
                        let pipelines = r.pipelines.lock().unwrap_or_else(|e| e.into_inner());
                        text.push_str(&format!(", its pipelines {pipelines:?}"));
                    }
                }
                plain = p.pNext;
            }
        }
        text
    }

    /// Moves the debug layer's messages into our log; returns the first
    /// error among them.
    pub(crate) fn log_messages(&self) -> Option<String> {
        let Some(queue) = &self.messages else {
            return None;
        };
        let mut first_error = None;
        // SAFETY: each message is read into a buffer of the size the queue
        // reports, then the queue is emptied.
        unsafe {
            for i in 0..queue.GetNumStoredMessages() {
                let mut size = 0;
                if queue.GetMessage(i, None, &mut size).is_err() || size == 0 {
                    continue;
                }
                let mut buffer = vec![0u64; size.div_ceil(8)];
                let message = buffer.as_mut_ptr().cast::<D3D12_MESSAGE>();
                if queue.GetMessage(i, Some(message), &mut size).is_ok() {
                    let m = &*message;
                    let text = std::slice::from_raw_parts(m.pDescription, m.DescriptionByteLength);
                    let text = String::from_utf8_lossy(text);
                    let text = text.trim_end_matches('\0');
                    tracing::warn!(id = m.ID.0, severity = m.Severity.0, "D3D12: {text}");
                    if m.Severity.0 <= D3D12_MESSAGE_SEVERITY_ERROR.0 && first_error.is_none() {
                        first_error = Some(text.to_owned());
                    }
                }
            }
            queue.ClearStoredMessages();
        }
        first_error
    }

    /// A host render target view of `resource`, valid until it is recorded.
    pub(crate) fn rtv(
        &self,
        resource: &ID3D12Resource,
        desc: Option<&D3D12_RENDER_TARGET_VIEW_DESC>,
    ) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        let slot = self.next_rtv.fetch_add(1, Ordering::Relaxed) % RTV_RING;
        // SAFETY: a slot of our heap and a live resource.
        unsafe {
            let mut handle = self.rtvs.GetCPUDescriptorHandleForHeapStart();
            handle.ptr += (slot * self.rtv_size) as usize;
            self.device
                .CreateRenderTargetView(resource, desc.map(|d| d as *const _), handle);
            handle
        }
    }

    /// A host depth view of `resource`, valid until it is recorded.
    pub(crate) fn dsv(
        &self,
        resource: &ID3D12Resource,
        desc: Option<&D3D12_DEPTH_STENCIL_VIEW_DESC>,
    ) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        let slot = self.next_dsv.fetch_add(1, Ordering::Relaxed) % RTV_RING;
        // SAFETY: a slot of our heap and a live resource.
        unsafe {
            let mut handle = self.dsvs.GetCPUDescriptorHandleForHeapStart();
            handle.ptr += (slot * self.dsv_size) as usize;
            self.device
                .CreateDepthStencilView(resource, desc.map(|d| d as *const _), handle);
            handle
        }
    }
}

impl HostList {
    fn continuations(&self) -> std::sync::MutexGuard<'_, Vec<Continuation>> {
        self.continuations.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn recording(&self) -> Option<(Arc<HostList>, bool)> {
        if !self.has_buffer_copies.load(Ordering::Relaxed) {
            return None;
        }
        let mut parts = self.continuations();
        match parts.last()? {
            Continuation::Commands(list) => return Some((list.clone(), false)),
            Continuation::Copy(_) | Continuation::Wait(_) => {}
        }
        let current = parts
            .iter()
            .rev()
            .find_map(|part| match part {
                Continuation::Commands(list) => Some(list.as_ref()),
                Continuation::Copy(_) | Continuation::Wait(_) => None,
            })
            .unwrap_or(self);
        let mut next = host().unwrap().new_list().unwrap_or_else(|e| {
            fail(format_args!(
                "a buffer copy continuation did not open ({e})"
            ))
        });
        next.budget = current.budget.clone();
        let next = Arc::new(next);
        parts.push(Continuation::Commands(next.clone()));
        Some((next, true))
    }

    pub(crate) fn mark_buffer_access(&self) {
        self.may_access_buffers.store(true, Ordering::Relaxed);
    }

    fn may_access_buffers(&self) -> bool {
        self.may_access_buffers.load(Ordering::Relaxed) || !self.deferred().copies.is_empty()
    }

    /// Ends native recording before an ordered title buffer copy. A
    /// continuation is only created if more commands are recorded.
    pub(crate) fn buffer_copy(
        &self,
        dst: crate::object::This,
        to: usize,
        src: crate::object::This,
        from: usize,
        len: usize,
    ) {
        if len == 0 || to == from {
            return;
        }
        if overlaps((to, len), (from, len)) {
            tracing::warn!(to, from, len, "overlapping title buffer copy");
            return;
        }
        let mut parts = self.continuations();
        self.end_segment(&parts);
        parts.push(Continuation::Copy(Arc::new(BufferCopy {
            to,
            from,
            len,
            buffers: [
                crate::resource::BufferLease::new(src),
                crate::resource::BufferLease::new(dst),
            ],
            native: OnceLock::new(),
        })));
        self.has_buffer_copies.store(true, Ordering::Relaxed);
    }

    /// Ends native recording before a title wait: the commands before it
    /// run before it is waited for, those after it in a continuation.
    pub(crate) fn wait(&self, mut wait: MemoryWait) {
        wait.writes_before = self.deferred().writes.len();
        let mut parts = self.continuations();
        self.end_segment(&parts);
        parts.push(Continuation::Wait(wait));
        self.has_buffer_copies.store(true, Ordering::Relaxed);
    }

    /// Closes the native list being recorded, if one is.
    fn end_segment(&self, parts: &[Continuation]) {
        match parts.last() {
            Some(Continuation::Commands(current)) => current.close_native(),
            Some(Continuation::Copy(_) | Continuation::Wait(_)) => {}
            None => self.close_native(),
        }
    }

    /// The timestamp heap and its read-back buffer (diagnostics, timing on).
    #[cfg(feature = "diagnostics")]
    fn stamps(&self) -> Option<&(ID3D12QueryHeap, ID3D12Resource)> {
        if !timing() {
            return None;
        }
        self.stamps
            .get_or_init(|| {
                let device = &host()?.device;
                let mut heap: Option<ID3D12QueryHeap> = None;
                let mut back: Option<ID3D12Resource> = None;
                let readback = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_READBACK, ..Default::default() };
                let desc = D3D12_RESOURCE_DESC {
                    Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                    Width: u64::from(STAMPS) * 8,
                    Height: 1,
                    DepthOrArraySize: 1,
                    MipLevels: 1,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                    ..Default::default()
                };
                // SAFETY: plain object creation.
                unsafe {
                    device
                        .CreateQueryHeap(
                            &D3D12_QUERY_HEAP_DESC { Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP, Count: STAMPS, NodeMask: 0 },
                            &mut heap,
                        )
                        .ok()?;
                    device
                        .CreateCommittedResource(
                            &readback,
                            D3D12_HEAP_FLAG_NONE,
                            &desc,
                            D3D12_RESOURCE_STATE_COPY_DEST,
                            None,
                            &mut back,
                        )
                        .ok()?;
                }
                Some((heap?, back?))
            })
            .as_ref()
    }

    /// The noted commands that took longest on the GPU, longest first, as
    /// (milliseconds, text) (diagnostics, timing on, after the list ran).
    #[cfg(feature = "diagnostics")]
    fn slowest(&self, frequency: u64, n: usize) -> Vec<(f64, String)> {
        let notes = self.notes.lock().unwrap_or_else(|e| e.into_inner());
        let (Some((_, back)), count) = (self.stamps(), notes.len().min(STAMPS as usize - 1)) else {
            return Vec::new();
        };
        let mut data = std::ptr::null_mut();
        let range = D3D12_RANGE { Begin: 0, End: (count + 1) * 8 };
        // SAFETY: a read-back buffer the finished list resolved into.
        let stamps: Vec<u64> = unsafe {
            if back.Map(0, Some(&range), Some(&mut data)).is_err() {
                return Vec::new();
            }
            let s = std::slice::from_raw_parts(data.cast::<u64>(), count + 1).to_vec();
            back.Unmap(0, None);
            s
        };
        let mut times: Vec<(f64, String)> = (0..count)
            .map(|i| {
                let ticks = stamps[i + 1].saturating_sub(stamps[i]);
                (ticks as f64 * 1e3 / frequency.max(1) as f64, notes[i].0.chars().take(300).collect())
            })
            .collect();
        times.sort_by(|a, b| b.0.total_cmp(&a.0));
        times.truncate(n);
        times
    }

    pub(crate) fn close(&self) {
        self.end_segment(&self.continuations());
    }

    fn close_native(&self) {
        #[cfg(feature = "diagnostics")]
        if let Some((heap, back)) = self.stamps() {
            let count = (self.notes.lock().unwrap_or_else(|e| e.into_inner()).len() as u32).min(STAMPS - 1);
            // SAFETY: an open list, our heap and read-back buffer.
            unsafe {
                self.list.EndQuery(heap, D3D12_QUERY_TYPE_TIMESTAMP, count);
                self.list.ResolveQueryData(heap, D3D12_QUERY_TYPE_TIMESTAMP, 0, count + 1, back, 0);
            }
        }
        // SAFETY: our list.
        if let Err(e) = unsafe { self.list.Close() } {
            fail(format_args!(
                "a host command list did not close ({e}): a command recorded into it was invalid"
            ));
        }
    }

    /// Only after the list's last submission finished (every one is waited
    /// for).
    pub(crate) fn reset(&self) {
        self.continuations().clear();
        self.has_buffer_copies.store(false, Ordering::Relaxed);
        *self.deferred() = Deferred::default();
        self.states
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.pipelines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        #[cfg(feature = "diagnostics")]
        self.notes.lock().unwrap_or_else(|e| e.into_inner()).clear();
        {
            let mut scratch = self.scratch.lock().unwrap_or_else(|e| e.into_inner());
            (scratch.current, scratch.used) = (0, 0);
        }
        {
            let mut arguments = self.arguments.lock().unwrap_or_else(|e| e.into_inner());
            (arguments.filled, arguments.used) = (0, 0);
        }
        // SAFETY: the list is not executing.
        let reset = unsafe {
            self.allocator
                .Reset()
                .and_then(|()| self.list.Reset(&self.allocator, None))
        };
        if let Err(e) = reset {
            fail(format_args!("a host command list did not reset ({e})"));
        }
    }

    pub(crate) fn reset_title(&self) {
        self.reset();
        self.may_access_buffers.store(false, Ordering::Relaxed);
    }

    /// Moves a title texture's host copy to `wanted` (a host state) in this
    /// list: the host texture and the state it leaves, if that is a change.
    /// The list's first use of a texture records nothing: its prelude brings
    /// the texture there at submission.
    pub(crate) fn transition(
        &self,
        resource: crate::object::This,
        wanted: u32,
    ) -> Option<(&'static ID3D12Resource, u32)> {
        let texture = crate::resource::host_resource(resource)?;
        let copy = crate::resource::host_copy(resource)?;
        let wanted = legal_state(texture, wanted);
        let mut states = self.states.lock().unwrap_or_else(|e| e.into_inner());
        match states.entry(std::sync::Arc::as_ptr(&copy) as usize) {
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert((wanted, wanted, copy));
                None
            }
            std::collections::hash_map::Entry::Occupied(mut o) => {
                let before = std::mem::replace(&mut o.get_mut().1, wanted);
                (before != wanted).then_some((texture, before))
            }
        }
    }

    /// At submission: the list that moves each texture this list uses from
    /// its real state to the one the list starts with, copying the staging
    /// buffers of title textures whose host copy lacks their bytes (after
    /// the list's refreshes); the textures' real states become those the
    /// list ends with.
    fn prelude(&self) -> Option<ID3D12CommandList> {
        const COPY_DEST: u32 = 0x400;
        let uploads: Vec<_> = self
            .deferred()
            .copies
            .iter()
            .filter_map(|step| match step {
                Step::Texture(staging, copy) if staging.upload_due(&copy.staged) => {
                    Some((staging.clone(), copy.clone()))
                }
                _ => None,
            })
            .collect();
        let uploading = |copy: &crate::resource::HostTexture| {
            uploads.iter().any(|(_, c)| std::ptr::eq(c.as_ref(), copy))
        };
        let states = self.states.lock().unwrap_or_else(|e| e.into_inner());
        // Barriers before the copies and after them.
        let (mut before, mut after) = (Vec::new(), Vec::new());
        let barrier = |copy: &crate::resource::HostTexture, from: u32, to: u32| {
            transition(
                &copy.texture,
                D3D12_RESOURCE_STATES(from as i32),
                D3D12_RESOURCE_STATES(to as i32),
            )
        };
        for (first, last, copy) in states.values() {
            let real = copy.state.swap(*last, Ordering::Relaxed);
            if uploading(copy) {
                let dest = legal_state(&copy.texture, COPY_DEST);
                if real != dest {
                    before.push(barrier(copy, real, dest));
                }
                if dest != *first {
                    after.push(barrier(copy, dest, *first));
                }
            } else if real != *first {
                before.push(barrier(copy, real, *first));
            }
        }
        // A texture uploaded but not otherwise used stays a copy destination.
        for (_, copy) in &uploads {
            if !states.contains_key(&(std::sync::Arc::as_ptr(copy) as usize)) {
                let dest = legal_state(&copy.texture, COPY_DEST);
                let real = copy.state.swap(dest, Ordering::Relaxed);
                if real != dest {
                    before.push(barrier(copy, real, dest));
                }
            }
        }
        if before.is_empty() && uploads.is_empty() {
            return None;
        }
        let (allocator, list) = &self.prelude;
        // SAFETY: the prelude is not executing (every submission is waited
        // for); the textures and staging buffers are live.
        let recorded = unsafe {
            allocator.Reset().and_then(|()| {
                list.Reset(allocator, None)?;
                if !before.is_empty() {
                    list.ResourceBarrier(&before);
                }
                for (staging, copy) in &uploads {
                    staging.record_upload(list, &copy.texture);
                }
                if !after.is_empty() {
                    list.ResourceBarrier(&after);
                }
                list.Close()
            })
        };
        if let Err(e) = recorded {
            fail(format_args!("a host state prelude did not record ({e})"));
        }
        list.cast().ok()
    }

    /// Notes what the command about to be recorded does, as a marker
    /// command whose text the list keeps (diagnostics, with device-removed
    /// data on): the report pairs the breadcrumbs' marker commands with the
    /// texts in order and names the commands a fault stopped among.
    #[cfg(feature = "diagnostics")]
    pub(crate) fn note_op(&self, what: impl FnOnce() -> String) {
        self.note(what, None);
    }

    /// As [`HostList::note_op`], for a copy that reads the host GPU range
    /// `(address, bytes)`: the report places the faulting address in it.
    #[cfg(feature = "diagnostics")]
    pub(crate) fn note_read(&self, what: impl FnOnce() -> String, range: (u64, u64)) {
        self.note(what, Some(range));
    }

    #[cfg(feature = "diagnostics")]
    fn note(&self, what: impl FnOnce() -> String, range: Option<(u64, u64)>) {
        if dred_on() {
            let at = self.notes.lock().unwrap_or_else(|e| e.into_inner()).len() as u32;
            if let Some((heap, _)) = self.stamps() && at < STAMPS - 1 {
                // SAFETY: an open list and our query heap.
                unsafe { self.list.EndQuery(heap, D3D12_QUERY_TYPE_TIMESTAMP, at) };
            }
            let what = what();
            let text: Vec<u16> = what.encode_utf16().chain([0]).collect();
            const UNICODE: u32 = 0;
            // SAFETY: an open list; the marker is copied when recorded.
            unsafe {
                self.list.SetMarker(UNICODE, Some(text.as_ptr().cast()), 2 * text.len() as u32)
            };
            self.notes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((what, range));
        }
    }

    /// Notes a pipeline the list binds.
    pub(crate) fn used_pipeline(&self, name: &str) {
        let mut pipelines = self.pipelines.lock().unwrap_or_else(|e| e.into_inner());
        if !pipelines.contains(name) {
            pipelines.insert(name.to_owned());
        }
    }

    fn deferred(&self) -> std::sync::MutexGuard<'_, Deferred> {
        self.deferred.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Copies `len` bytes from `src` to `dst` when the list is submitted.
    pub(crate) fn defer_copy(&self, dst: *mut u8, src: *const u8, len: usize) {
        let mut deferred = self.deferred();
        deferred
            .copies
            .push(Step::Copy(dst as usize, src as usize, len));
        deferred.refreshed.clear();
        deferred.copy_epoch += 1;
    }

    /// Copies `len` bytes of title memory at `src` into the list's upload
    /// memory at `dst` when the list is submitted.
    pub(crate) fn defer_fill(&self, dst: *mut u8, src: *const u8, len: usize) {
        self.deferred()
            .copies
            .push(Step::Fill(dst as usize, src as usize, len));
    }

    /// Brings `[address, address + len)` of a buffer's host copy up to date
    /// when the list is submitted (once per range until a title copy
    /// intervenes), keeping the copy alive until the list is reset.
    pub(crate) fn defer_refresh(
        &self,
        shadow: &std::sync::Arc<crate::shadow::Shadow>,
        address: usize,
        len: usize,
    ) {
        let mut deferred = self.deferred();
        let key = (std::sync::Arc::as_ptr(shadow) as usize, address, len);
        if deferred.refreshed.insert(key) {
            deferred
                .copies
                .push(Step::Refresh(shadow.clone(), address, len));
        }
    }

    /// Shaders of the list write `[address, address + len)` of a buffer's
    /// host copy: copied back into title memory once the list has run.
    pub(crate) fn defer_gpu_write(
        &self,
        shadow: &std::sync::Arc<crate::shadow::Shadow>,
        address: usize,
        len: usize,
    ) {
        self.deferred()
            .copies
            .push(Step::GpuWrite(shadow.clone(), address, len));
    }

    /// Stores `what` (`bytes` of it) at `dst` once the list has run.
    pub(crate) fn defer_write(&self, dst: *mut u8, what: crate::queue::Written, bytes: usize) {
        note_write(self, dst as usize, what);
        self.deferred().writes.push((dst as usize, what, bytes));
    }

    /// A mapped upload buffer of `size` bytes, alive until the list is
    /// reset.
    ///
    /// shortcut: one buffer per copy; pool them once uploads are frequent.
    pub(crate) fn upload(&self, host: &Host, size: u64) -> Option<(ID3D12Resource, *mut u8)> {
        let (buffer, data) = host.upload_buffer(size)?;
        name(&buffer, || format!("list upload {size:#x}"));
        self.deferred().uploads.push(buffer.clone());
        Some((buffer, data))
    }

    /// `len` bytes of upload memory (256-byte aligned) until the list is
    /// reset: the host GPU address and the CPU pointer.
    pub(crate) fn scratch(&self, len: u64) -> Option<(u64, *mut u8)> {
        let host = host()?;
        let mut s = self.scratch.lock().unwrap_or_else(|e| e.into_inner());
        let len = len.max(1).next_multiple_of(256);
        if len > SCRATCH_CHUNK {
            let (buffer, data) = host.upload_buffer(len)?;
            name(&buffer, || format!("list scratch {len:#x}"));
            // SAFETY: a live buffer.
            let gpu = unsafe { buffer.GetGPUVirtualAddress() };
            self.deferred().uploads.push(buffer);
            return Some((gpu, data));
        }
        if s.used + len > SCRATCH_CHUNK && s.current < s.chunks.len() {
            (s.current, s.used) = (s.current + 1, 0);
        }
        if s.current == s.chunks.len() {
            let (buffer, data) = host.upload_buffer(SCRATCH_CHUNK)?;
            name(&buffer, || "list scratch".into());
            // SAFETY: a live buffer.
            let gpu = unsafe { buffer.GetGPUVirtualAddress() };
            s.chunks.push((buffer, data as usize, gpu));
            s.used = 0;
        }
        let (_, data, gpu) = s.chunks[s.current];
        let at = s.used;
        s.used += len;
        Some((gpu + at, (data + at as usize) as *mut u8))
    }

    /// The list's indirect-work budget, zeroed before each submission.
    pub(crate) fn budget(&self) -> Option<&ID3D12Resource> {
        self.budget
            .get_or_init(|| {
                let (b, d) = host()?.shared_buffer(256)?;
                Some((b, d as usize))
            })
            .as_ref()
            .map(|(buffer, _)| buffer)
    }

    pub(crate) fn use_indirect_buffers<'a>(
        &self,
        buffers: impl IntoIterator<Item = &'a ID3D12Resource>,
    ) {
        let mut deferred = self.deferred();
        for buffer in buffers {
            deferred
                .indirect_buffers
                .entry(buffer.as_raw() as usize)
                .or_insert_with(|| buffer.clone());
        }
    }

    /// `len` bytes (at most 64 KiB) the list's shaders write and its
    /// indirect executions read: a buffer and the offset in it, the list's
    /// until it is reset. The buffer starts each submission in the common
    /// state.
    pub(crate) fn argument_space(&self, len: u64) -> Option<(ID3D12Resource, u64)> {
        if len > ARGUMENT_CHUNK {
            return None;
        }
        let mut a = self.arguments.lock().unwrap_or_else(|e| e.into_inner());
        if a.filled == 0 || a.used + len > ARGUMENT_CHUNK {
            if a.filled == a.chunks.len() {
                let (buffer, _) = host()?.shared_buffer(ARGUMENT_CHUNK)?;
                name(&buffer, || "indirect arguments".into());
                a.chunks.push(buffer);
            }
            a.filled += 1;
            a.used = 0;
        }
        let at = a.used;
        a.used += len.next_multiple_of(16);
        Some((a.chunks[a.filled - 1].clone(), at))
    }

    /// Keeps `object` alive until the list is reset.
    pub(crate) fn keep(&self, object: &impl Interface) {
        if let Ok(object) = object.cast::<ID3D12Object>() {
            self.deferred().kept.push(object);
        }
    }

    /// Whether this is the first time since the reset that the list asks
    /// about `key` (a texture to bring up to date).
    pub(crate) fn first_use(&self, key: usize) -> bool {
        self.deferred().textures.insert(key)
    }

    /// Runs `work` when the list is submitted, in order with its copies.
    /// `work` may write title memory (refreshes after it copy again).
    pub(crate) fn defer(&self, work: impl Fn() + Send + 'static) {
        let mut deferred = self.deferred();
        deferred.copies.push(Step::Run(Box::new(work)));
        deferred.refreshed.clear();
        deferred.copy_epoch += 1;
    }

    /// Epoch of title-memory-writing steps recorded into this list so far.
    pub(crate) fn title_write_epoch(&self) -> u64 {
        self.deferred().copy_epoch
    }

    /// Queues submission-time materialization and refresh of the bindless
    /// slots in `heap`, once per list; `writes`: the draw's shaders may write
    /// memory, so the slots' unordered access ranges go back to title memory.
    pub(crate) fn defer_bindless(
        &self,
        heap: &std::sync::Arc<crate::descriptor::host::HostHeap>,
        writes: bool,
    ) {
        let mut deferred = self.deferred();
        match deferred
            .bindless
            .iter_mut()
            .find(|(h, _)| std::sync::Arc::ptr_eq(h, heap))
        {
            Some((_, w)) => *w |= writes,
            None => deferred.bindless.push((heap.clone(), writes)),
        }
    }

    /// Whether each heap queued for bindless preparation may be written.
    #[cfg(test)]
    pub(crate) fn bindless_queued(&self) -> Vec<bool> {
        self.deferred().bindless.iter().map(|&(_, w)| w).collect()
    }

    /// As [`HostList::defer`], for work that writes only host memory.
    pub(crate) fn defer_host(&self, read: (usize, usize), work: impl Fn() + Send + 'static) {
        self.deferred()
            .copies
            .push(Step::Host(Some(read), Box::new(work)));
    }

    /// Brings a texture's staging buffer up to date when the list is
    /// submitted, and its host copy from it in the list's prelude, keeping
    /// both alive until the list is reset.
    pub(crate) fn defer_texture(
        &self,
        staging: std::sync::Arc<crate::texture::Staging>,
        copy: std::sync::Arc<crate::resource::HostTexture>,
    ) {
        self.deferred().copies.push(Step::Texture(staging, copy));
    }

    fn needs_title_bytes(&self, pending: &[ShadowRange]) -> bool {
        let depends = |range| {
            pending
                .iter()
                .any(|(_, from, to)| overlaps(range, (*from, to - from)))
        };
        self.deferred().copies.iter().any(|step| match *step {
            Step::Fill(_, src, len) | Step::Host(Some((src, len)), _) => depends((src, len)),
            // A mapped fallback changes title bytes. Keep its refreshes
            // after prior GPU readers as well as prior GPU writers.
            Step::Copy(..) => true,
            // Overlapping title ranges can resolve to separate native
            // shadows. Refresh an alias after the producer's copyback.
            Step::Refresh(ref shadow, at, len) => pending.iter().any(|(other, from, to)| {
                !Arc::ptr_eq(shadow, other) && overlaps((at, len), (*from, to - from))
            }),
            Step::Texture(ref staging, _) => depends(staging.title_range()),
            Step::Run(_) => !pending.is_empty(),
            _ => false,
        })
    }

    fn add_gpu_writes(&self, pending: &mut Vec<ShadowRange>) {
        for step in &self.deferred().copies {
            if let Step::GpuWrite(shadow, address, len) = step {
                pending.push((shadow.clone(), *address, address + len));
            }
        }
    }

    /// Runs the list's submission-time steps; refreshes go into `batch`
    /// (the title's big buffers, a 174 MB one among them, are read by most
    /// lists of a submission, through many ranges).
    fn run_deferred(&self, batch: &mut Batch) {
        #[cfg(feature = "diagnostics")]
        use crate::copystat::{C, add};
        let deferred = self.deferred();
        for (heap, writes) in &deferred.bindless {
            batch.bindless(heap, *writes);
        }
        for step in &deferred.copies {
            match step {
                &Step::Copy(dst, src, len) => {
                    batch.title_written();
                    run_mapped_copy(dst, src, len, true);
                }
                &Step::Fill(dst, src, len) => {
                    run_mapped_copy(dst, src, len, false);
                }
                Step::Refresh(shadow, address, len) => {
                    #[cfg(feature = "diagnostics")]
                    {
                        add(C::Refreshes, 1);
                        add(C::RefreshBytes, *len as u64);
                    }
                    batch.add(shadow, *address, *len);
                }
                Step::Run(work) | Step::Host(_, work) => {
                    #[cfg(feature = "diagnostics")]
                    let started = std::time::Instant::now();
                    if matches!(step, Step::Run(_)) {
                        batch.title_written();
                    }
                    work();
                    #[cfg(feature = "diagnostics")]
                    {
                        add(C::Runs, 1);
                        add(C::RunUs, started.elapsed().as_micros() as u64);
                    }
                }
                Step::GpuWrite(shadow, address, len) => {
                    batch.queue_write(shadow, *address, *len);
                }
                Step::Texture(staging, _) => {
                    if batch.texture_due(staging) {
                        staging.refresh();
                    }
                }
            }
        }
    }
}

/// What, after a title wait in this submission, may write its memory: a
/// bottom-of-pipe write of a later list or segment, a shader write or a
/// title buffer copy.
fn released_later(
    runs: &[&HostList],
    continuations: &[Vec<Continuation>],
    after: &[(&HostList, QueueSegment)],
    run: usize,
    part: usize,
    wait: &MemoryWait,
) -> Option<&'static str> {
    let range = (wait.address, wait.len());
    let mine = {
        let deferred = runs[run].deferred();
        wait.written_by(&deferred.writes[wait.writes_before.min(deferred.writes.len())..])
    };
    if mine || runs[run + 1..].iter().any(|l| wait.written_by(&l.deferred().writes)) {
        return Some("bottom-of-pipe write");
    }
    if after.iter().any(|(l, _)| {
        l.deferred().copies.iter().any(|step| match *step {
            Step::GpuWrite(_, at, len) | Step::Copy(at, _, len) => overlaps((at, len), range),
            _ => false,
        })
    }) {
        return Some("shader write or title copy");
    }
    continuations[run][part + 1..]
        .iter()
        .chain(continuations[run + 1..].iter().flatten())
        .any(|c| matches!(c, Continuation::Copy(copy) if overlaps((copy.to, copy.len), range)))
        .then_some("buffer copy")
}

/// Whether a wait message is logged: the first 20, then powers of two.
fn wait_logged(count: &std::sync::atomic::AtomicU64) -> Option<u64> {
    let n = count.fetch_add(1, Ordering::Relaxed) + 1;
    (n <= 20 || n.is_power_of_two()).then_some(n)
}

/// A title wait released only later in its own submission: not waited for.
fn wait_not_waited(wait: &MemoryWait, what: &str) {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    if let Some(n) = wait_logged(&N) {
        tracing::warn!(
            "Wait{}BitValueX at {:#x} (comparison {}, value {:#x}; memory holds {:x?}) is released by a {what} later in the same submission: not waited for ({n}){}",
            wait.len() * 8,
            wait.address,
            wait.comparison,
            wait.value,
            wait.read(),
            write_history(wait.address)
        );
    }
}

/// A title wait did not hold in time: the list went on.
fn wait_timed_out(wait: &MemoryWait, waited: std::time::Duration) {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    if let Some(n) = wait_logged(&N) {
        tracing::warn!(
            "Wait{}BitValueX timed out after {} ms at {:#x} (comparison {}, value {:#x}, mask {:#x}; memory holds {:x?}); the list goes on (timeout {n}){}",
            wait.len() * 8,
            waited.as_millis(),
            wait.address,
            wait.comparison,
            wait.value,
            wait.mask,
            wait.read(),
            write_history(wait.address)
        );
    }
}

/// Bottom-of-pipe writes recorded per address (diagnostics): what, by which
/// list, on which thread, when, and whether it was done yet.
#[cfg(feature = "diagnostics")]
type WriteSeen = (crate::queue::Written, usize, String, std::time::Instant, bool);
#[cfg(feature = "diagnostics")]
static WRITES_SEEN: Mutex<Option<std::collections::HashMap<usize, WriteSeen>>> = Mutex::new(None);

/// Notes a recorded bottom-of-pipe write (diagnostics).
fn note_write(list: &HostList, at: usize, what: crate::queue::Written) {
    #[cfg(feature = "diagnostics")]
    {
        let thread = std::thread::current();
        let thread = thread.name().map_or_else(|| format!("{:?}", thread.id()), str::to_owned);
        let mut seen = WRITES_SEEN.lock().unwrap_or_else(|e| e.into_inner());
        seen.get_or_insert_with(Default::default).insert(
            at,
            (what, std::ptr::from_ref(list) as usize, thread, std::time::Instant::now(), false),
        );
    }
    #[cfg(not(feature = "diagnostics"))]
    let _ = (list, at, what);
}

/// Does a list's bottom-of-pipe write once it has run.
fn perform_write(at: usize, what: crate::queue::Written, bytes: usize) {
    what.write(at as *mut u8, bytes);
    #[cfg(feature = "diagnostics")]
    if let Some(seen) = WRITES_SEEN.lock().unwrap_or_else(|e| e.into_inner()).as_mut()
        && let Some(entry) = seen.get_mut(&at)
    {
        entry.4 = true;
    }
}

/// The last bottom-of-pipe write recorded to `address`, for wait reports
/// (diagnostics builds; empty otherwise).
fn write_history(address: usize) -> String {
    #[cfg(feature = "diagnostics")]
    {
        let seen = WRITES_SEEN.lock().unwrap_or_else(|e| e.into_inner());
        match seen.as_ref().and_then(|s| s.get(&address)) {
            Some((what, list, thread, when, done)) => format!(
                "; last write recorded there: {what:?} by list {list:#x} on thread {thread} {} ms ago, {}",
                when.elapsed().as_millis(),
                if *done { "done" } else { "not run yet" }
            ),
            None => "; no bottom-of-pipe write was ever recorded there".to_owned(),
        }
    }
    #[cfg(not(feature = "diagnostics"))]
    {
        let _ = address;
        String::new()
    }
}

fn run_mapped_copy(dst: usize, src: usize, len: usize, title_dst: bool) {
    #[cfg(feature = "diagnostics")]
    {
        use crate::copystat::{C, add};
        add(C::Copies, 1);
        add(C::CopyBytes, len as u64);
    }
    let started = std::time::Instant::now();
    copy_mapped(dst as *mut u8, src as *const u8, len, title_dst);
    PREPARING[0].fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
    PREPARING[1].fetch_add(len as u64, Ordering::Relaxed);
}

fn overlaps((a, a_len): (usize, usize), (b, b_len): (usize, usize)) -> bool {
    a_len != 0 && b_len != 0 && a < b.saturating_add(b_len) && b < a.saturating_add(a_len)
}

/// A submission's refreshes not done yet, per buffer copy, merged where
/// they share or touch a 64 KiB page: done before a step that writes title
/// memory or gives a range to shaders, and before the lists run. Ranges
/// pages apart stay apart: one range covering a big buffer's low and high
/// ends would have its untouched middle checked (and its stale pages
/// copied) too.
///
/// Also the textures brought up to date in the submission so far: most
/// lists of a world submission sample the same textures, and a texture in
/// physical-page memory is hashed whole each time it is checked. Checked
/// once, it is checked again only after a step that writes title memory.
#[derive(Default)]
struct Batch {
    /// Per copy, `[from, to)` ranges in the order they came.
    refreshes: Vec<(std::sync::Arc<crate::shadow::Shadow>, Ranges)>,
    /// Ranges shaders write, bindless ones included, copied back after the
    /// batch ran.
    writes: Vec<ShadowRange>,
    /// Staging buffers (by address) refreshed since the last title write.
    textures: std::collections::HashSet<usize>,
    /// View heaps the submission's draws index directly, and whether their
    /// shaders may write memory.
    bindless: Vec<(std::sync::Arc<crate::descriptor::host::HostHeap>, bool)>,
    /// Their plans, which hold the host resources the slots view until the
    /// submission has run.
    plans: Vec<std::sync::Arc<crate::descriptor::host::BindlessPlan>>,
}

/// `[from, to)` ranges of title memory.
type Ranges = Vec<(usize, usize)>;

/// Sorts `[from, to)` ranges and merges those whose `page`-sized pages
/// overlap or touch; empty ranges go.
fn coalesce_ranges(ranges: &mut Ranges, page: usize) {
    ranges.retain(|&(from, to)| from < to);
    ranges.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for &(from, to) in ranges.iter() {
        match merged.last_mut() {
            Some((_, end)) if from / page <= end.div_ceil(page) => *end = (*end).max(to),
            _ => merged.push((from, to)),
        }
    }
    *ranges = merged;
}

type ShadowRange = (Arc<crate::shadow::Shadow>, usize, usize);

struct GpuWrites {
    ranges: Vec<ShadowRange>,
    snapshots: Vec<ShadowRange>,
}

impl GpuWrites {
    fn copy_back(self) -> usize {
        let written = self
            .ranges
            .into_iter()
            .map(|(shadow, from, to)| shadow.copy_back(from, to - from))
            .sum();
        for (shadow, ..) in self.snapshots {
            shadow.finish_writes();
        }
        written
    }
}

impl Batch {
    fn queue_write(&mut self, shadow: &Arc<crate::shadow::Shadow>, from: usize, len: usize) {
        self.flush();
        shadow.queue_write(from, len);
        self.writes.push((shadow.clone(), from, from + len));
    }

    fn snapshot_writes(&mut self) -> GpuWrites {
        let mut writes = std::mem::take(&mut self.writes);
        let mut extents = std::collections::HashMap::new();
        for (shadow, from, to) in &writes {
            let (_, first, end) = extents
                .entry(Arc::as_ptr(shadow))
                .or_insert_with(|| (shadow.clone(), *from, *to));
            *first = (*first).min(*from);
            *end = (*end).max(*to);
        }
        let mut snapshots: Vec<_> = extents.into_values().collect();
        snapshots.sort_unstable_by_key(|(_, from, _)| *from);
        for (shadow, from, to) in &snapshots {
            shadow.snapshot(*from, to - from);
        }
        // Distinct shadows can cover the same title address. Preserve
        // declaration order in that case so the later copyback wins.
        if snapshots.windows(2).any(|pair| pair[0].2 > pair[1].1) {
            let mut seen = std::collections::HashSet::new();
            writes.retain(|(s, from, to)| seen.insert((Arc::as_ptr(s), *from, *to)));
            return GpuWrites {
                ranges: writes,
                snapshots,
            };
        }
        writes.sort_unstable_by_key(|(shadow, from, _)| (Arc::as_ptr(shadow), *from));
        let mut merged: Vec<ShadowRange> = Vec::new();
        for (shadow, from, to) in writes {
            if let Some((previous, _, end)) = merged.last_mut()
                && Arc::ptr_eq(previous, &shadow)
                && from <= *end
            {
                *end = (*end).max(to);
            } else {
                merged.push((shadow, from, to));
            }
        }
        GpuWrites {
            ranges: merged,
            snapshots,
        }
    }

    fn add(&mut self, shadow: &std::sync::Arc<crate::shadow::Shadow>, address: usize, len: usize) {
        if len == 0 {
            return;
        }
        let range = (address, address.saturating_add(len));
        match self
            .refreshes
            .iter_mut()
            .find(|(s, _)| std::sync::Arc::ptr_eq(s, shadow))
        {
            Some((_, ranges)) => ranges.push(range),
            None => self.refreshes.push((shadow.clone(), vec![range])),
        }
    }

    /// Whether `staging` is still to be refreshed in this submission (and
    /// notes it refreshed).
    fn texture_due<T>(&mut self, staging: &std::sync::Arc<T>) -> bool {
        self.textures.insert(std::sync::Arc::as_ptr(staging) as *const () as usize)
    }

    /// A list of the submission indexes `heap` directly.
    fn bindless(&mut self, heap: &std::sync::Arc<crate::descriptor::host::HostHeap>, writes: bool) {
        match self
            .bindless
            .iter_mut()
            .find(|(h, _)| std::sync::Arc::ptr_eq(h, heap))
        {
            Some((_, w)) => *w |= writes,
            None => self.bindless.push((heap.clone(), writes)),
        }
    }

    /// Before a step that writes title memory: pending refreshes are done
    /// first, and textures are checked again after it.
    fn title_written(&mut self) {
        self.flush();
        self.textures.clear();
    }

    fn flush(&mut self) {
        for (shadow, mut ranges) in self.refreshes.drain(..) {
            coalesce_ranges(&mut ranges, crate::watch::PAGE);
            for (from, to) in ranges {
                let started = std::time::Instant::now();
                let copied = shadow.update(from, to - from);
                let us = started.elapsed().as_micros() as u64;
                #[cfg(feature = "diagnostics")]
                crate::copystat::range(from, to - from, us, 0);
                PREPARING[0].fetch_add(us, Ordering::Relaxed);
                PREPARING[1].fetch_add(copied as u64, Ordering::Relaxed);
            }
        }
    }

    /// After every list's steps (no title copy follows): each directly
    /// indexed heap's slots, once per submission however many lists and
    /// draws use it. What they read is refreshed (unchanged pages cost only
    /// the write-watch's answer); their unordered access ranges are kept
    /// for copy-back only when a shader that may write memory uses the heap.
    fn prepare_bindless(&mut self) {
        self.flush();
        for (heap, writes) in std::mem::take(&mut self.bindless) {
            let Some(plan) = heap.bindless_plan() else {
                continue;
            };
            plan.copy_constants();
            for (shadow, address, len) in &plan.reads {
                #[cfg(feature = "diagnostics")]
                {
                    use crate::copystat::{C, add};
                    add(C::Refreshes, 1);
                    add(C::RefreshBytes, *len as u64);
                }
                update(shadow, *address, *len);
            }
            if writes {
                for (shadow, address, len) in &plan.writes {
                    if !self.writes.iter().any(|(s, a, end)| {
                        Arc::ptr_eq(s, shadow) && (*a, *end) == (*address, address + len)
                    }) {
                        self.queue_write(shadow, *address, *len);
                    }
                }
            }
            self.plans.push(plan);
        }
    }
}

/// Brings `[from, from + len)` of a host copy up to date now.
fn update(shadow: &crate::shadow::Shadow, from: usize, len: usize) {
    let started = std::time::Instant::now();
    let copied = shadow.update(from, len);
    let us = started.elapsed().as_micros() as u64;
    #[cfg(feature = "diagnostics")]
    crate::copystat::range(from, len, us, 0);
    PREPARING[0].fetch_add(us, Ordering::Relaxed);
    PREPARING[1].fetch_add(copied as u64, Ordering::Relaxed);
}

/// Copies title memory at submission. The ranges were checked against their
/// resources when recorded, but the title may since have unmapped pages of
/// them (tiled memory, page mappings), which read as zeros here; a title
/// destination (`title_dst`) that is not all mapped takes nothing from its
/// first unmapped page on.
///
/// The copy runs in the kernel (`ReadProcessMemory`), which reports an
/// unmapped page of the source, or of the destination it writes, instead of
/// faulting. Asking `VirtualQuery` first cost about 2 ms per copy in the
/// title's big pools (it walks the page tables to the region's end), which
/// made copies run at 100-200 MB/s; a title destination is asked only once
/// its copy has failed (asked before each, the world's 80 copies per frame
/// cost 60 ms).
pub(crate) fn copy_mapped(dst: *mut u8, src: *const u8, len: usize, title_dst: bool) {
    #[cfg(feature = "diagnostics")]
    match crate::copystat::mode() {
        1 => {
            // SAFETY: timing only: the caller vouches for the source.
            unsafe { std::ptr::copy(src, dst, len) };
            return;
        }
        2 => return,
        _ => {}
    }
    if len == 0 {
        return;
    }
    if title_dst {
        // The kernel writes the destination: it must not be guarded.
        crate::watch::writing(dst as usize, len);
    }
    #[cfg(feature = "diagnostics")]
    let moved = std::time::Instant::now();
    let whole = read(src, dst, len);
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(crate::copystat::C::MoveUs, moved.elapsed().as_micros() as u64);
    if whole {
        return;
    }
    // The copy stopped at an unmapped page. One of a title destination ends
    // it: the retries below write the destination themselves (zeros for
    // what cannot be read), and would fault there.
    if title_dst && !crate::texture::committed(dst, len) {
        return;
    }
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(crate::copystat::C::Fallbacks, 1);
    if !title_dst && len >= 64 << 10 {
        read_sparse(src, dst, len);
    } else {
        read_pieces(src, dst, len);
    }
}

/// After a failed bulk read into host memory, skip contiguous holes. Never
/// cache the query across copies: the title may remap the same address.
fn read_sparse(src: *const u8, dst: *mut u8, len: usize) {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_NOACCESS, VirtualQuery,
    };
    let mut offset = 0;
    while offset < len {
        let at = src.wrapping_add(offset);
        let to = dst.wrapping_add(offset);
        let remaining = len - offset;
        // SAFETY: the query inspects an address and fills a local structure.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        let size = unsafe { VirtualQuery(at.cast(), &mut info, size_of_val(&info)) };
        let n = (info.BaseAddress as usize).saturating_add(info.RegionSize)
            .saturating_sub(at as usize).min(remaining);
        if size != size_of_val(&info) || n == 0 {
            read_pieces(at, to, remaining);
            break;
        }
        if info.State != MEM_COMMIT || info.Protect == PAGE_NOACCESS {
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::Unmapped, n as u64);
            // SAFETY: this path is only used with a writable host destination.
            unsafe { to.write_bytes(0, n) };
        } else if !read(at, to, n) {
            // A mapping may change between query and read.
            read_pieces(at, to, n);
        }
        offset += n;
    }
}

fn read_pieces(src: *const u8, dst: *mut u8, len: usize) {
    // Something is unmapped: page by page (64 KiB, then 4 KiB in a piece
    // that fails), zeros for what cannot be read.
    for_pieces(src as usize, len, 64 << 10, |at, n| {
        let to = dst.wrapping_add(at - src as usize);
        if !read(at as *const u8, to, n) {
            for_pieces(at, n, 4 << 10, |at, n| {
                let to = dst.wrapping_add(at - src as usize);
                if !read(at as *const u8, to, n) {
                    #[cfg(feature = "diagnostics")]
                    crate::copystat::add(crate::copystat::C::Unmapped, n as u64);
                    // SAFETY: inside the destination, which is mapped.
                    unsafe { to.write_bytes(0, n) };
                }
            });
        }
    });
}

/// Reads title memory at `src` into `dst`; false if a page of it is not
/// readable.
pub(crate) fn read_into(src: *const u8, dst: &mut [u8]) -> bool {
    read(src, dst.as_mut_ptr(), dst.len())
}

/// Copies `len` bytes from `src` to `dst` in the kernel: false if a source
/// page is not readable or a destination page not writable (the bytes
/// before that page may have been copied).
fn read(src: *const u8, dst: *mut u8, len: usize) -> bool {
    use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut done = 0;
    // SAFETY: the destination is `len` bytes the caller may write, host
    // memory or title memory; the kernel checks both ranges, and a page it
    // cannot read or write fails the call instead of faulting.
    unsafe { ReadProcessMemory(GetCurrentProcess(), src.cast(), dst.cast(), len, &mut done) != 0 }
}

/// Copies host memory into title memory after a submission (what shaders
/// wrote), leaving pages that are not mapped alone.
///
/// Through [`read`]: the kernel refuses an unmapped destination page as it
/// does a source page. `WriteProcessMemory` makes the same copy, but asks
/// for the destination's region first, at `VirtualQuery`'s cost (see
/// [`copy_mapped`]): 0.35 ms for each piece of the world's copy-back, 39 ms
/// per frame.
pub(crate) fn write_mapped(dst: *mut u8, src: *const u8, len: usize) {
    let write = |at: usize, n: usize| read(src.wrapping_add(at - dst as usize), at as *mut u8, n);
    if len == 0 || write(dst as usize, len) {
        return;
    }
    for_pieces(dst as usize, len, 64 << 10, |at, n| {
        if !write(at, n) {
            for_pieces(at, n, 4 << 10, |at, n| {
                write(at, n);
            });
        }
    });
}

/// Calls `f` with the pieces of `[start, start + len)` cut at multiples of
/// `step`.
fn for_pieces(start: usize, len: usize, step: usize, mut f: impl FnMut(usize, usize)) {
    let end = start + len;
    let mut at = start;
    while at < end {
        let next = ((at / step + 1) * step).min(end);
        f(at, next - at);
        at = next;
    }
}

#[cfg(test)]
mod title_copies {
    use super::{copy_mapped, write_mapped};
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_NOACCESS, PAGE_READONLY, PAGE_READWRITE,
        VirtualAlloc, VirtualFree, VirtualProtect,
    };

    const BLOCK: usize = 64 << 10;

    /// Three reserved blocks, the first and the last committed and filled
    /// with `fill`: title memory with an unmapped block in it.
    fn holed(fill: u8) -> *mut u8 {
        // SAFETY: a fresh reservation; the committed blocks are ours to fill.
        unsafe {
            let base =
                VirtualAlloc(std::ptr::null(), 3 * BLOCK, MEM_RESERVE, PAGE_NOACCESS).cast::<u8>();
            assert!(!base.is_null());
            for block in [0, 2] {
                let at = base.add(block * BLOCK);
                assert!(!VirtualAlloc(at.cast(), BLOCK, MEM_COMMIT, PAGE_READWRITE).is_null());
                at.write_bytes(fill, BLOCK);
            }
            base
        }
    }

    fn block<'a>(base: *const u8, n: usize) -> &'a [u8] {
        // SAFETY: tests ask for committed blocks only.
        unsafe { std::slice::from_raw_parts(base.add(n * BLOCK), BLOCK) }
    }

    fn release(base: *mut u8) {
        // SAFETY: a reservation of `holed`.
        assert_ne!(unsafe { VirtualFree(base.cast(), 0, MEM_RELEASE) }, 0);
    }

    fn pattern() -> Vec<u8> {
        (0..3 * BLOCK).map(|i| (i % 251) as u8 + 1).collect()
    }

    #[test]
    fn title_copies_reach_mapped_destinations_and_stop_at_unmapped_pages() {
        let from = pattern();
        // All mapped: copied, through a destination that was never asked.
        let to = holed(0xee);
        copy_mapped(to, from.as_ptr(), BLOCK, true);
        assert_eq!(block(to, 0), &from[..BLOCK]);
        release(to);
        // An unmapped block in the destination: nothing faults, and nothing
        // arrives from that block on.
        let to = holed(0xee);
        copy_mapped(to, from.as_ptr(), 3 * BLOCK, true);
        assert!(block(to, 2).iter().all(|&b| b == 0xee));
        release(to);
        // An unmapped block in the source reads as zeros.
        let holes = holed(7);
        let mut to = vec![0xeeu8; 3 * BLOCK];
        copy_mapped(to.as_mut_ptr(), holes, 3 * BLOCK, true);
        assert!(to[..BLOCK].iter().all(|&b| b == 7));
        assert!(to[BLOCK..2 * BLOCK].iter().all(|&b| b == 0));
        assert!(to[2 * BLOCK..].iter().all(|&b| b == 7));
        release(holes);
    }

    #[test]
    fn copy_back_writes_mapped_pages_only() {
        let from = pattern();
        let to = holed(0xee);
        write_mapped(to, from.as_ptr(), 3 * BLOCK);
        assert_eq!(block(to, 0), &from[..BLOCK]);
        assert_eq!(block(to, 2), &from[2 * BLOCK..]);
        release(to);
        // Neither aligned nor whole: the bytes around the range stay.
        let to = holed(0xee);
        // SAFETY: inside the first block.
        write_mapped(unsafe { to.add(100) }, from.as_ptr(), BLOCK - 300);
        let first = block(to, 0);
        assert_eq!(first[100..BLOCK - 200], from[..BLOCK - 300]);
        let around = first[..100].iter().chain(&first[BLOCK - 200..]);
        assert!(around.into_iter().all(|&b| b == 0xee));
        // A page that is mapped but not writable is not written either
        // (guarded title blocks are opened before they are copied back).
        let mut old = 0;
        // SAFETY: a committed block of ours.
        let protected =
            unsafe { VirtualProtect(to.add(2 * BLOCK).cast(), BLOCK, PAGE_READONLY, &mut old) };
        assert_ne!(protected, 0);
        // SAFETY: the third block.
        write_mapped(unsafe { to.add(2 * BLOCK) }, from.as_ptr(), BLOCK);
        assert!(block(to, 2).iter().all(|&b| b == 0xee));
        release(to);
    }
}

impl Host {
    /// A buffer in the upload heap, mapped for writing.
    pub(crate) fn upload_buffer(&self, size: u64) -> Option<(ID3D12Resource, *mut u8)> {
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_UPLOAD,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size.max(1),
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let mut buffer: Option<ID3D12Resource> = None;
        let mut data = std::ptr::null_mut();
        // SAFETY: plain creation and mapping of an upload buffer.
        let made = unsafe {
            self.device
                .CreateCommittedResource(
                    &heap,
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    D3D12_RESOURCE_STATE_GENERIC_READ,
                    None,
                    &mut buffer,
                )
                .and_then(|()| {
                    let b = buffer.as_ref().ok_or_else(windows::core::Error::empty)?;
                    b.Map(0, None, Some(&mut data))
                })
        };
        match made {
            Ok(()) => Some((buffer?, data.cast())),
            Err(e) => {
                tracing::warn!(error = %e, size, "no upload buffer");
                None
            }
        }
    }
}

/// The swap chain formats a presented texture can be copied into: its own
/// family's.
fn swap_format(format: DXGI_FORMAT) -> Option<DXGI_FORMAT> {
    Some(match format.0 {
        27..=32 => DXGI_FORMAT_R8G8B8A8_UNORM,
        87 | 90 | 91 => DXGI_FORMAT_B8G8R8A8_UNORM,
        23..=25 => DXGI_FORMAT_R10G10B10A2_UNORM,
        9..=14 => DXGI_FORMAT_R16G16B16A16_FLOAT,
        _ => return None,
    })
}

const BUFFERS: u32 = 2;
const MAX_PLANES: u32 = 8;

/// Built-in fullscreen-triangle compositor for multi-plane `PresentX` calls
/// (e.g. Series S presenting a 1080p scene plane with a 4K UI overlay plane).
struct Compositor {
    format: DXGI_FORMAT,
    root: ID3D12RootSignature,
    opaque: ID3D12PipelineState,
    overlay: ID3D12PipelineState,
    rtv_heap: ID3D12DescriptorHeap,
    srv_heap: ID3D12DescriptorHeap,
    srv_stride: u32,
}

impl Compositor {
    fn new(device: &ID3D12Device, format: DXGI_FORMAT) -> windows::core::Result<Self> {
        const SOURCE: &str = r#"
            #define RS "DescriptorTable(SRV(t0), visibility=SHADER_VISIBILITY_PIXEL), \
                        StaticSampler(s0, filter=FILTER_MIN_MAG_MIP_LINEAR, \
                            addressU=TEXTURE_ADDRESS_CLAMP, addressV=TEXTURE_ADDRESS_CLAMP, \
                            visibility=SHADER_VISIBILITY_PIXEL)"
            Texture2D tex : register(t0);
            SamplerState smp : register(s0);
            [RootSignature(RS)]
            void vs(uint id : SV_VertexID, out float4 pos : SV_Position, out float2 uv : TEXCOORD) {
                uv = float2((id << 1) & 2, id & 2);
                pos = float4(uv * float2(2, -2) + float2(-1, 1), 0, 1);
            }
            [RootSignature(RS)]
            float4 ps(float4 pos : SV_Position, float2 uv : TEXCOORD) : SV_Target {
                return tex.Sample(smp, uv);
            }
        "#;
        let compile = |entry: PCSTR, target: PCSTR| -> windows::core::Result<ID3DBlob> {
            let mut code: Option<ID3DBlob> = None;
            let mut errors: Option<ID3DBlob> = None;
            // SAFETY: static source and entry/target strings; out pointers ours.
            let res = unsafe {
                D3DCompile(
                    SOURCE.as_ptr().cast(),
                    SOURCE.len(),
                    s!("present_composite"),
                    None,
                    None,
                    entry,
                    target,
                    0,
                    0,
                    &mut code,
                    Some(&mut errors),
                )
            };
            if let Err(e) = res {
                if let Some(errors) = errors {
                    // SAFETY: the blob's own bytes.
                    let text = unsafe {
                        std::slice::from_raw_parts(
                            errors.GetBufferPointer().cast::<u8>(),
                            errors.GetBufferSize(),
                        )
                    };
                    tracing::warn!("present compositor: {}", String::from_utf8_lossy(text));
                }
                return Err(e);
            }
            code.ok_or_else(windows::core::Error::empty)
        };
        let vs = compile(s!("vs"), s!("vs_5_1"))?;
        let ps = compile(s!("ps"), s!("ps_5_1"))?;
        // SAFETY: compiled shader blobs live through PSO creation.
        unsafe {
            let vs_bytes =
                std::slice::from_raw_parts(vs.GetBufferPointer().cast::<u8>(), vs.GetBufferSize());
            let ps_bytes =
                std::slice::from_raw_parts(ps.GetBufferPointer().cast::<u8>(), ps.GetBufferSize());
            let root: ID3D12RootSignature = device.CreateRootSignature(0, vs_bytes)?;
            let make_pso = |blend: bool| -> windows::core::Result<ID3D12PipelineState> {
                let mut rt_blend = D3D12_RENDER_TARGET_BLEND_DESC {
                    RenderTargetWriteMask: D3D12_COLOR_WRITE_ENABLE_ALL.0 as u8,
                    ..Default::default()
                };
                if blend {
                    rt_blend.BlendEnable = true.into();
                    rt_blend.SrcBlend = D3D12_BLEND_ONE;
                    rt_blend.DestBlend = D3D12_BLEND_INV_SRC_ALPHA;
                    rt_blend.BlendOp = D3D12_BLEND_OP_ADD;
                    rt_blend.SrcBlendAlpha = D3D12_BLEND_ONE;
                    rt_blend.DestBlendAlpha = D3D12_BLEND_INV_SRC_ALPHA;
                    rt_blend.BlendOpAlpha = D3D12_BLEND_OP_ADD;
                }
                let mut blend_desc = D3D12_BLEND_DESC::default();
                blend_desc.RenderTarget[0] = rt_blend;
                let mut rtv_formats = [DXGI_FORMAT_UNKNOWN; 8];
                rtv_formats[0] = format;
                let desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
                    pRootSignature: std::mem::ManuallyDrop::new(Some(root.clone())),
                    VS: D3D12_SHADER_BYTECODE {
                        pShaderBytecode: vs_bytes.as_ptr().cast(),
                        BytecodeLength: vs_bytes.len(),
                    },
                    PS: D3D12_SHADER_BYTECODE {
                        pShaderBytecode: ps_bytes.as_ptr().cast(),
                        BytecodeLength: ps_bytes.len(),
                    },
                    BlendState: blend_desc,
                    SampleMask: u32::MAX,
                    RasterizerState: D3D12_RASTERIZER_DESC {
                        FillMode: D3D12_FILL_MODE_SOLID,
                        CullMode: D3D12_CULL_MODE_NONE,
                        DepthClipEnable: false.into(),
                        ..Default::default()
                    },
                    PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
                    NumRenderTargets: 1,
                    RTVFormats: rtv_formats,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    ..Default::default()
                };
                let pso = device.CreateGraphicsPipelineState(&desc);
                let _ = std::mem::ManuallyDrop::into_inner(desc.pRootSignature);
                pso
            };
            let opaque = make_pso(false)?;
            let overlay = make_pso(true)?;
            let rtv_heap: ID3D12DescriptorHeap =
                device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
                    NumDescriptors: 1,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_NONE,
                    NodeMask: 0,
                })?;
            let srv_heap: ID3D12DescriptorHeap =
                device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                    NumDescriptors: MAX_PLANES,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                    NodeMask: 0,
                })?;
            let srv_stride =
                device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV);
            Ok(Compositor {
                format,
                root,
                opaque,
                overlay,
                rtv_heap,
                srv_heap,
                srv_stride,
            })
        }
    }
}

struct Presenter {
    swap_chain: IDXGISwapChain3,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
    copy: HostList,
    hwnd: HWND,
    meter: Meter,
    compositor: Option<Compositor>,
}

/// The frame rate, shown in the window's caption every second and logged
/// every few seconds.
struct Meter {
    since: std::time::Instant,
    frames: u32,
    #[cfg(feature = "diagnostics")]
    copy_stats_frames: u64,
    drawn: u64,
    logged: std::time::Instant,
    /// The caption the window had, once read.
    caption: Option<Vec<u16>>,
}

impl Meter {
    fn new() -> Self {
        let now = std::time::Instant::now();
        Meter {
            since: now,
            frames: 0,
            #[cfg(feature = "diagnostics")]
            copy_stats_frames: 0,
            drawn: crate::draw::drawn(),
            logged: now,
            caption: None,
        }
    }

    /// Counts a presented frame.
    fn frame(&mut self, hwnd: HWND) {
        #[cfg(feature = "diagnostics")]
        tracing::debug!(target: "victory_gpu::frame", token = crate::frame::current(), "present");
        self.frames += 1;
        #[cfg(feature = "diagnostics")]
        {
            self.copy_stats_frames += 1;
        }
        let elapsed = self.since.elapsed();
        if elapsed < std::time::Duration::from_secs(1) {
            return;
        }
        let drawn = crate::draw::drawn();
        let frames = f64::from(self.frames);
        let fps = frames / elapsed.as_secs_f64();
        let ms = elapsed.as_secs_f64() * 1000.0 / frames;
        let draws = (drawn - self.drawn) as f64 / frames;
        (self.since, self.frames, self.drawn) = (std::time::Instant::now(), 0, drawn);
        // Of a frame, ms preparing submissions and ms waiting for the GPU.
        let [cpu, gpu] = [0, 1].map(|i| {
            SUBMISSION_US[i].swap(0, Ordering::Relaxed) as f64 / 1000.0 / frames
        });
        let [copy_ms, copy_mb, texture_ms, texture_mb] = [0, 1, 2, 3].map(|i| {
            let scale = if i % 2 == 0 { 1e3 } else { 1e6 };
            PREPARING[i].swap(0, Ordering::Relaxed) as f64 / scale / frames
        });
        let [back_ms, back_mb] = [0, 1].map(|i| {
            COPY_BACK[i].swap(0, Ordering::Relaxed) as f64 / [1e3, 1e6][i] / frames
        });
        let dropped = crate::draw::take_dropped() as f64 / frames;
        let indirect = crate::list::take_indirect() as f64 / frames;
        // Over the same frames as the counts above.
        #[cfg(feature = "diagnostics")]
        let drops = crate::pipeline::take_drops();
        if self.logged.elapsed() >= std::time::Duration::from_secs(5) {
            self.logged = std::time::Instant::now();
            tracing::info!(
                "{fps:.1} fps, {ms:.1} ms/frame ({cpu:.1} preparing: copies {copy_ms:.1} ms \
                 {copy_mb:.1} MB, textures {texture_ms:.1} ms {texture_mb:.1} MB; {gpu:.1} on the \
                 GPU; {back_ms:.1} copying back {back_mb:.1} MB), {draws:.0} draws/frame, \
                 dropped {dropped:.0} without a host pipeline ({making} being made) and \
                 {indirect:.0} indirect",
                making = crate::pipeline::making()
            );
            let (peak, short) = crate::descriptor::host::take_dynamic_stats();
            let [not_made, taken_back, refused] = crate::draw::take_table_stats();
            if peak > 0 {
                tracing::info!(
                    "dynamic descriptor slots: at most {peak} taken; {short} requests ran short, \
                     {taken_back} submitted lists' slots taken back, {not_made} draws or dispatches \
                     dropped for their tables, {refused} lists not run again"
                );
            }
            #[cfg(feature = "diagnostics")]
            {
                crate::copystat::log(std::mem::take(&mut self.copy_stats_frames) as f64);
                for (why, n) in drops.iter().take(40) {
                    tracing::info!(
                        "dropped {:.1}/frame without a host pipeline: {why}",
                        *n as f64 / frames
                    );
                }
            }
        }
        if self.caption.is_none() {
            // The window's own text, read without a message.
            let mut buffer = vec![0u16; 512];
            // SAFETY: a window handle and a buffer of that length.
            let n = unsafe { InternalGetWindowText(hwnd, &mut buffer) };
            buffer.truncate(n.max(0) as usize);
            self.caption = Some(buffer);
        }
        let Some(caption) = &self.caption else { return };
        let text = format!(" | VIctory {fps:.0} fps {ms:.1} ms/frame {draws:.0} draws/frame");
        let mut text: Vec<u16> = caption.iter().copied().chain(text.encode_utf16()).collect();
        text.push(0);
        // Setting the text waits for the window's thread (a title thread,
        // which may not pump for a while), so a thread of ours does it and
        // presenting never waits; it takes the newest text when free.
        static PENDING: Mutex<Option<(usize, Vec<u16>)>> = Mutex::new(None);
        static SETTER: std::sync::Once = std::sync::Once::new();
        *PENDING.lock().unwrap_or_else(|e| e.into_inner()) = Some((hwnd.0 as usize, text));
        SETTER.call_once(|| {
            let _ = std::thread::Builder::new()
                .name("victory-caption".into())
                .spawn(|| {
                    loop {
                        let next = PENDING.lock().unwrap_or_else(|e| e.into_inner()).take();
                        match next {
                            // SAFETY: a window handle and a NUL-terminated text.
                            Some((hwnd, text)) => unsafe {
                                let _ = SetWindowTextW(HWND(hwnd as *mut _), PCWSTR(text.as_ptr()));
                            },
                            None => std::thread::sleep(std::time::Duration::from_millis(100)),
                        }
                    }
                });
        });
    }
}

impl Host {
    /// Shows one or more `planes` (each a live host texture and its current
    /// host state, kept in that state), compositing planes 1.. on top of
    /// plane 0 when multiple planes are presented, as frame `frame` (saved if
    /// captures are on). `None` if nothing was shown; else, in diagnostics
    /// builds, whether the frame was black.
    pub(crate) fn present(&self, planes: &[(&ID3D12Resource, u32)], frame: u64) -> Option<bool> {
        let &(primary, _) = planes.first()?;
        // SAFETY: live resources.
        let primary_desc = unsafe { primary.GetDesc() };
        let Some(format) = swap_format(primary_desc.Format) else {
            tracing::warn!(format = primary_desc.Format.0, "cannot present this format");
            return None;
        };
        let (mut width, mut height) = (primary_desc.Width as u32, primary_desc.Height);
        for &(tex, _) in &planes[1..] {
            // SAFETY: live resource.
            let d = unsafe { tex.GetDesc() };
            width = width.max(d.Width as u32);
            height = height.max(d.Height);
        }
        let mut presenter = self.presenter.lock().unwrap_or_else(|e| e.into_inner());
        if presenter.is_none() {
            *presenter = match self.presenter(width, height, format) {
                Ok(p) => Some(p),
                Err(e) => {
                    tracing::warn!(error = %e, "no swap chain");
                    return None;
                }
            };
        }
        let p = presenter.as_mut()?;
        if (p.width, p.height, p.format) != (width, height, format) {
            // SAFETY: no back buffer is held; every submission was waited for.
            let resized = unsafe {
                p.swap_chain
                    .ResizeBuffers(BUFFERS, width, height, format, DXGI_SWAP_CHAIN_FLAG(0))
            };
            match resized {
                Ok(()) => (p.width, p.height, p.format) = (width, height, format),
                Err(e) => {
                    tracing::warn!(error = %e, width, height, "swap chain not resized");
                    return None;
                }
            }
        }
        // SAFETY: our swap chain, list and live source textures.
        unsafe {
            let index = p.swap_chain.GetCurrentBackBufferIndex();
            let Ok(back) = p.swap_chain.GetBuffer::<ID3D12Resource>(index) else {
                return None;
            };
            p.copy.reset();
            let list = &p.copy.list;
            let (capture, probe) = if planes.len() == 1 {
                let (texture, state) = planes[0];
                let state = D3D12_RESOURCE_STATES(state as i32);
                let mut before = Vec::with_capacity(2);
                if state != D3D12_RESOURCE_STATE_COPY_SOURCE {
                    before.push(transition(texture, state, D3D12_RESOURCE_STATE_COPY_SOURCE));
                }
                before.push(transition(
                    &back,
                    D3D12_RESOURCE_STATE_PRESENT,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                ));
                list.ResourceBarrier(&before);
                list.CopyResource(&back, texture);
                #[cfg(feature = "diagnostics")]
                let capture = crate::capture::wanted(frame)
                    .then(|| crate::capture::read_back(&self.device, list, texture))
                    .flatten();
                #[cfg(feature = "diagnostics")]
                let probe = crate::capture::probe(&self.device, list, texture);
                #[cfg(not(feature = "diagnostics"))]
                let (capture, probe) = {
                    let _ = frame;
                    (None::<()>, None::<()>)
                };
                let mut after = Vec::with_capacity(2);
                if state != D3D12_RESOURCE_STATE_COPY_SOURCE {
                    after.push(transition(texture, D3D12_RESOURCE_STATE_COPY_SOURCE, state));
                }
                after.push(transition(
                    &back,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    D3D12_RESOURCE_STATE_PRESENT,
                ));
                list.ResourceBarrier(&after);
                (capture, probe)
            } else {
                if p.compositor.as_ref().is_none_or(|c| c.format != format) {
                    match Compositor::new(&self.device, format) {
                        Ok(c) => p.compositor = Some(c),
                        Err(e) => {
                            tracing::warn!(error = %e, "no present compositor");
                            return None;
                        }
                    }
                }
                let comp = p.compositor.as_ref()?;
                let rtv = comp.rtv_heap.GetCPUDescriptorHandleForHeapStart();
                self.device.CreateRenderTargetView(
                    &back,
                    Some(&D3D12_RENDER_TARGET_VIEW_DESC {
                        Format: format,
                        ViewDimension: D3D12_RTV_DIMENSION_TEXTURE2D,
                        Anonymous: D3D12_RENDER_TARGET_VIEW_DESC_0 {
                            Texture2D: D3D12_TEX2D_RTV {
                                MipSlice: 0,
                                PlaneSlice: 0,
                            },
                        },
                    }),
                    rtv,
                );
                let srv_cpu_base = comp.srv_heap.GetCPUDescriptorHandleForHeapStart();
                let srv_gpu_base = comp.srv_heap.GetGPUDescriptorHandleForHeapStart();
                let count = planes.len().min(MAX_PLANES as usize);
                let mut before = Vec::with_capacity(count + 1);
                for (i, &(tex, state)) in planes[..count].iter().enumerate() {
                    let d = tex.GetDesc();
                    let srv_fmt = swap_format(d.Format).unwrap_or(d.Format);
                    let handle = D3D12_CPU_DESCRIPTOR_HANDLE {
                        ptr: srv_cpu_base.ptr + i * comp.srv_stride as usize,
                    };
                    self.device.CreateShaderResourceView(
                        tex,
                        Some(&D3D12_SHADER_RESOURCE_VIEW_DESC {
                            Format: srv_fmt,
                            ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
                            Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                                Texture2D: D3D12_TEX2D_SRV {
                                    MostDetailedMip: 0,
                                    MipLevels: 1,
                                    PlaneSlice: 0,
                                    ResourceMinLODClamp: 0.0,
                                },
                            },
                        }),
                        handle,
                    );
                    let state = D3D12_RESOURCE_STATES(state as i32);
                    if state != D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE {
                        before.push(transition(
                            tex,
                            state,
                            D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
                        ));
                    }
                }
                before.push(transition(
                    &back,
                    D3D12_RESOURCE_STATE_PRESENT,
                    D3D12_RESOURCE_STATE_RENDER_TARGET,
                ));
                list.ResourceBarrier(&before);
                list.SetDescriptorHeaps(&[Some(comp.srv_heap.clone())]);
                list.SetGraphicsRootSignature(&comp.root);
                list.RSSetViewports(&[D3D12_VIEWPORT {
                    TopLeftX: 0.0,
                    TopLeftY: 0.0,
                    Width: width as f32,
                    Height: height as f32,
                    MinDepth: 0.0,
                    MaxDepth: 1.0,
                }]);
                list.RSSetScissorRects(&[RECT {
                    left: 0,
                    top: 0,
                    right: width as i32,
                    bottom: height as i32,
                }]);
                list.OMSetRenderTargets(1, Some(&rtv), false, None);
                list.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
                for i in 0..count {
                    list.SetPipelineState(if i == 0 { &comp.opaque } else { &comp.overlay });
                    list.SetGraphicsRootDescriptorTable(
                        0,
                        D3D12_GPU_DESCRIPTOR_HANDLE {
                            ptr: srv_gpu_base.ptr + (i as u64) * u64::from(comp.srv_stride),
                        },
                    );
                    list.DrawInstanced(3, 1, 0, 0);
                }
                #[cfg(feature = "diagnostics")]
                let (capture, probe) = {
                    list.ResourceBarrier(&[transition(
                        &back,
                        D3D12_RESOURCE_STATE_RENDER_TARGET,
                        D3D12_RESOURCE_STATE_COPY_SOURCE,
                    )]);
                    let capture = crate::capture::wanted(frame)
                        .then(|| crate::capture::read_back(&self.device, list, &back))
                        .flatten();
                    let probe = crate::capture::probe(&self.device, list, &back);
                    list.ResourceBarrier(&[transition(
                        &back,
                        D3D12_RESOURCE_STATE_COPY_SOURCE,
                        D3D12_RESOURCE_STATE_PRESENT,
                    )]);
                    (capture, probe)
                };
                #[cfg(not(feature = "diagnostics"))]
                let (capture, probe) = {
                    let _ = frame;
                    list.ResourceBarrier(&[transition(
                        &back,
                        D3D12_RESOURCE_STATE_RENDER_TARGET,
                        D3D12_RESOURCE_STATE_PRESENT,
                    )]);
                    (None::<()>, None::<()>)
                };
                let mut after = Vec::with_capacity(count);
                for &(tex, state) in &planes[..count] {
                    let state = D3D12_RESOURCE_STATES(state as i32);
                    if state != D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE {
                        after.push(transition(
                            tex,
                            D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
                            state,
                        ));
                    }
                }
                if !after.is_empty() {
                    list.ResourceBarrier(&after);
                }
                (capture, probe)
            };
            p.copy.close();
            self.execute(&[&p.copy]);
            #[cfg(feature = "diagnostics")]
            if let Some((buffer, footprint)) = capture {
                crate::capture::save_read_back(frame, &buffer, &footprint);
            }
            let _ = p.swap_chain.Present(0, DXGI_PRESENT(0));
            p.meter.frame(p.hwnd);
            #[cfg(feature = "diagnostics")]
            return Some(probe.is_some_and(|b| crate::capture::black(&b)));
            #[cfg(not(feature = "diagnostics"))]
            {
                let _ = (capture, probe);
                Some(false)
            }
        }
    }

    fn presenter(
        &self,
        width: u32,
        height: u32,
        format: DXGI_FORMAT,
    ) -> windows::core::Result<Presenter> {
        let hwnd = match title_window() {
            Some(hwnd) => hwnd,
            None => window(width, height)?,
        };
        // SAFETY: plain object creation for our window.
        unsafe {
            let factory: IDXGIFactory2 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))?;
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: width,
                Height: height,
                Format: format,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: BUFFERS,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                ..Default::default()
            };
            let swap_chain = factory
                .CreateSwapChainForHwnd(&self.queue, hwnd, &desc, None, None)?
                .cast::<IDXGISwapChain3>()?;
            let copy = self.new_list()?;
            copy.close();
            tracing::info!(width, height, format = format.0, "presenting to a window");
            Ok(Presenter {
                swap_chain,
                width,
                height,
                format,
                copy,
                hwnd,
                meter: Meter::new(),
                compositor: None,
            })
        }
    }
}

/// A visible top-level window of this process: the one the title made, if
/// it made one.
fn title_window() -> Option<HWND> {
    extern "system" fn visit(hwnd: HWND, found: LPARAM) -> windows::core::BOOL {
        let mut process = 0;
        // SAFETY: a window handle from the enumeration; `found` is ours.
        unsafe {
            GetWindowThreadProcessId(hwnd, Some(&mut process));
            if process == windows::Win32::System::Threading::GetCurrentProcessId()
                && IsWindowVisible(hwnd).as_bool()
            {
                *(found.0 as *mut Option<HWND>) = Some(hwnd);
                return false.into();
            }
        }
        true.into()
    }
    let mut found: Option<HWND> = None;
    // SAFETY: the callback only writes `found`, which outlives the call.
    let _ = unsafe { EnumWindows(Some(visit), LPARAM(&raw mut found as isize)) };
    found
}

/// `state` without the bits the host texture's flags forbid: the console
/// lets the title name states its own resources allow (unordered access on
/// a depth target, whose host copy cannot have both).
pub(crate) fn legal_state(texture: &ID3D12Resource, state: u32) -> u32 {
    // SAFETY: a plain query.
    legal_for(unsafe { texture.GetDesc() }.Flags.0 as u32, state)
}

/// [`legal_state`] for a texture with resource flags `flags`.
pub(crate) fn legal_for(flags: u32, state: u32) -> u32 {
    // (flag, set or clear forbids, state bits): render target, unordered
    // access, depth, denied shader resource.
    const FORBIDDEN: [(u32, bool, u32); 4] = [
        (0x1, false, 0x4),
        (0x4, false, 0x8),
        (0x2, false, 0x10 | 0x20),
        (0x8, true, 0x40 | 0x80),
    ];
    FORBIDDEN
        .iter()
        .filter(|&&(flag, deny, _)| (flags & flag != 0) == deny)
        .fold(state, |s, &(_, _, bits)| s & !bits)
}

impl Host {
    /// A buffer in system memory that the CPU maps (write-back, so reading
    /// it is fast too) and shaders read and write: for host copies of title
    /// memory that shaders write, which upload memory cannot be. Starts in
    /// the common state (buffers move out of it implicitly).
    pub(crate) fn shared_buffer(&self, size: u64) -> Option<(ID3D12Resource, *mut u8)> {
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_CUSTOM,
            CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_WRITE_BACK,
            MemoryPoolPreference: D3D12_MEMORY_POOL_L0,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size.max(1),
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            ..Default::default()
        };
        let mut buffer: Option<ID3D12Resource> = None;
        let mut data = std::ptr::null_mut();
        // SAFETY: plain creation and mapping of a CPU-visible buffer.
        let made = unsafe {
            self.device
                .CreateCommittedResource(
                    &heap,
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    D3D12_RESOURCE_STATE_COMMON,
                    None,
                    &mut buffer,
                )
                .and_then(|()| {
                    let b = buffer.as_ref().ok_or_else(windows::core::Error::empty)?;
                    b.Map(0, None, Some(&mut data))
                })
        };
        match made {
            Ok(()) => Some((buffer?, data.cast())),
            Err(e) => {
                tracing::warn!(error = %e, size, "no shared buffer");
                None
            }
        }
    }
}

/// For adapters without device-removed data (the software one): the last
/// commands each list of the failed submission noted (diagnostics, with
/// `VICTORY_D3D12_DRED`).
fn last_notes(runs: &[&HostList]) -> String {
    #[cfg(feature = "diagnostics")]
    {
        let mut text = String::new();
        for (i, r) in runs.iter().enumerate() {
            let notes = r.notes.lock().unwrap_or_else(|e| e.into_inner());
            for (n, _) in notes.iter().rev().take(6).rev() {
                text.push_str(&format!("; list {i} of {}, of its {}: {n}", runs.len(), notes.len()));
            }
        }
        text
    }
    #[cfg(not(feature = "diagnostics"))]
    {
        let _ = runs;
        String::new()
    }
}

/// Where the faulting address `va` lies against a range `(start, bytes)` a
/// command reads (diagnostics).
#[cfg(feature = "diagnostics")]
fn fault_in(va: u64, (start, bytes): (u64, u64)) -> String {
    let end = start + bytes;
    if va < start {
        format!(" (fault {:#x} below its source)", start - va)
    } else if va < end {
        format!(" (fault at source +{:#x})", va - start)
    } else {
        format!(" (fault {:#x} past its source's end)", va - end)
    }
}

/// The commands that may have been running where a list stopped at
/// command `done`: those between the barriers around it (work between
/// barriers overlaps on the GPU), by kind, and what the markers just
/// before them say (`HostList::note_op`: diagnostics builds with
/// device-removed data on). Up to 40 are named, from a few before the stop
/// on: in a long run between barriers the first commands after the barrier
/// have usually finished.
fn in_flight(history: &[D3D12_AUTO_BREADCRUMB_OP], markers: &[(u32, String)], done: usize) -> String {
    let barrier = |op: &D3D12_AUTO_BREADCRUMB_OP| *op == D3D12_AUTO_BREADCRUMB_OP_RESOURCEBARRIER;
    let from = history[..done].iter().rposition(barrier).map_or(0, |b| b + 1);
    let to = history[done..]
        .iter()
        .position(barrier)
        .map_or(history.len(), |b| (done + b).max(done + 1));
    let mut text = format!(", commands {from}..{to} between its barriers:");
    let mut shown = 0;
    for i in from.max(done.saturating_sub(4))..to {
        let op = history[i];
        if op == D3D12_AUTO_BREADCRUMB_OP_SETMARKER || shown == 40 {
            continue;
        }
        shown += 1;
        let marker = i
            .checked_sub(1)
            .filter(|&m| history[m] == D3D12_AUTO_BREADCRUMB_OP_SETMARKER)
            .and_then(|m| markers.iter().find(|(at, _)| *at as usize == m))
            .map_or("no marker", |(_, s)| s.as_str());
        text.push_str(&format!("; command {i} (op {}): {marker}", op.0));
    }
    text
}

pub(crate) fn transition(
    resource: &ID3D12Resource,
    before: D3D12_RESOURCE_STATES,
    after: D3D12_RESOURCE_STATES,
) -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: std::mem::ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                // SAFETY: a borrowed pointer; the barrier never releases it.
                pResource: unsafe { std::mem::transmute_copy(resource) },
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: before,
                StateAfter: after,
            }),
        },
    }
}

/// A window of the given client size, owned by its own message thread.
fn window(width: u32, height: u32) -> windows::core::Result<HWND> {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("victory-window".into())
        .spawn(move || {
            let made = make_window(width, height);
            let ok = made.is_ok();
            let _ = sender.send(made.map(|h| h.0 as usize));
            if ok {
                pump();
            }
        })
        .map_err(|_| windows::core::Error::empty())?;
    let hwnd = receiver
        .recv()
        .map_err(|_| windows::core::Error::empty())??;
    Ok(HWND(hwnd as *mut _))
}

extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: the default handling for our window.
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

fn make_window(width: u32, height: u32) -> windows::core::Result<HWND> {
    const CLASS: PCWSTR = w!("VIctory");
    // SAFETY: registers our class once and creates a window of it.
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(window_proc),
            hInstance: instance.into(),
            lpszClassName: CLASS,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            ..Default::default()
        };
        RegisterClassExW(&class);
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: width as i32,
            bottom: height as i32,
        };
        AdjustWindowRect(&mut rect, WS_OVERLAPPEDWINDOW, false)?;
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            CLASS,
            w!("VIctory"),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            rect.right - rect.left,
            rect.bottom - rect.top,
            None,
            None,
            Some(instance.into()),
            None,
        )
    }
}

fn pump() {
    let mut message = MSG::default();
    // SAFETY: the standard message loop of this thread's window.
    unsafe {
        while GetMessageW(&mut message, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

#[cfg(test)]
mod batch {   // Batch::texture_due lives here; `mod tests` is already taken at the top of this file
    use super::{Batch, coalesce_ranges};
    use std::sync::Arc;

    const PAGE: usize = crate::watch::PAGE;

    fn coalesced(mut ranges: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
        coalesce_ranges(&mut ranges, PAGE);
        ranges
    }

    #[test]
    fn empty_ranges_are_dropped() {
        assert_eq!(coalesced(vec![]), []);
        assert_eq!(coalesced(vec![(5, 5), (PAGE, PAGE)]), []);
        assert_eq!(coalesced(vec![(10, 10), (20, 30)]), [(20, 30)]);
    }

    #[test]
    fn ranges_in_one_page_merge() {
        assert_eq!(coalesced(vec![(100, 200), (10, 20), (150, 400)]), [(10, 400)]);
        // Nested and repeated.
        assert_eq!(coalesced(vec![(0, PAGE), (8, 16), (0, PAGE)]), [(0, PAGE)]);
    }

    #[test]
    fn ranges_in_touching_pages_merge() {
        // Ends in page 0, starts in page 1.
        assert_eq!(coalesced(vec![(PAGE + 8, PAGE + 16), (0, 8)]), [(0, PAGE + 16)]);
        // Ends exactly at page 1's start, starts in page 1.
        assert_eq!(coalesced(vec![(0, PAGE), (PAGE + 100, 2 * PAGE)]), [(0, 2 * PAGE)]);
        // A chain of ranges each in the page after the last.
        let chain: Vec<_> = (0..8).map(|p| (p * PAGE + 10, p * PAGE + 20)).collect();
        assert_eq!(coalesced(chain), [(10, 7 * PAGE + 20)]);
    }

    #[test]
    fn ranges_pages_apart_stay_apart() {
        // A whole untouched page between them.
        assert_eq!(coalesced(vec![(2 * PAGE, 2 * PAGE + 1), (0, 1)]), [(0, 1), (2 * PAGE, 2 * PAGE + 1)]);
        // The low and high ends of a 256 MiB buffer.
        let (low, high) = (1 << 30, (1 << 30) + (256 << 20) - 64);
        assert_eq!(
            coalesced(vec![(high, high + 64), (low, low + 64), (low + 32, low + 128)]),
            [(low, low + 128), (high, high + 64)]
        );
    }

    #[test]
    fn ranges_at_the_top_of_the_address_space_do_not_overflow() {
        let top = usize::MAX;
        assert_eq!(coalesced(vec![(top - 8, top), (top - 100, top - 50)]), [(top - 100, top)]);
    }

    #[test]
    fn a_texture_is_checked_once_per_submission() {
        let mut batch = Batch::default();
        let (a, b) = (Arc::new([0u8; 16]), Arc::new([0u8; 16]));
        // The first list sampling each texture checks it ...
        assert!(batch.texture_due(&a));
        assert!(batch.texture_due(&b));
        // ... later lists of the same submission do not.
        assert!(!batch.texture_due(&a));
        assert!(!batch.texture_due(&a.clone()));
        assert!(!batch.texture_due(&b));
    }

    #[test]
    fn title_writes_make_textures_due_again() {
        let mut batch = Batch::default();
        let a = Arc::new(1u32);
        assert!(batch.texture_due(&a));
        assert!(!batch.texture_due(&a));
        // A copy into title memory or work that may write it may have
        // changed the texture's memory.
        batch.title_written();
        assert!(batch.texture_due(&a));
        assert!(!batch.texture_due(&a));
        // A fresh submission starts over.
        let mut next = Batch::default();
        assert!(next.texture_due(&a));
    }

    /// The hashing a submission does for textures in physical-page memory,
    /// with every list checking the textures it samples (before) and with
    /// each checked once (after). Run with
    /// `cargo test --release -p victory-gpu -- --ignored --nocapture texture_checks`.
    #[test]
    #[ignore = "benchmark"]
    fn texture_checks_per_submission() {
        const TEXTURES: usize = 48;
        const SIZE: usize = 4 << 20;
        const LISTS: usize = 12;
        let textures: Vec<Arc<Vec<u8>>> = (0..TEXTURES)
            .map(|t| Arc::new((0..SIZE).map(|i| (i * 31 + t) as u8).collect()))
            .collect();
        let submit = |dedupe: bool| {
            let mut batch = Batch::default();
            let mut hashed = 0usize;
            let started = std::time::Instant::now();
            for _ in 0..LISTS {
                for t in &textures {
                    if !dedupe || batch.texture_due(t) {
                        std::hint::black_box(blake3::hash(t));
                        hashed += 1;
                    }
                }
            }
            (started.elapsed(), hashed)
        };
        submit(false); // warm up
        let (before, n_before) = submit(false);
        let (after, n_after) = submit(true);
        assert_eq!((n_before, n_after), (TEXTURES * LISTS, TEXTURES));
        println!(
            "{LISTS} lists sampling {TEXTURES} textures of {} MiB: \
             before {n_before} hashes in {before:?}, after {n_after} in {after:?} ({:.1}x)",
            SIZE >> 20,
            before.as_secs_f64() / after.as_secs_f64()
        );
    }
}
