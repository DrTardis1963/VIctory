//! Texture contents the title writes itself (decision 0007, "Texture
//! memory"): streamed pages, layout-library copies, movie frames, glyph
//! atlases. Such a texture lives in console memory in its tiled layout;
//! before a submission samples its host copy, the memory is detiled into a
//! staging buffer when its content changed, and the staging buffer is
//! copied into the host texture (in the list's prelude) unless the host
//! texture already holds it.
//!
//! "Changed" means the content hash of the whole memory differs from the
//! last detiling's; in write-watched memory it is taken only when a page of
//! the memory was written since ([`crate::watch`]), and once it was read
//! whole, of the written pages only, compared with the bytes read last; in
//! physical-page views, which cannot be watched, at every submission that
//! samples it.
//! Only the subresources whose memory holds changed bytes are detiled
//! again when the written pages are known (watched memory read before).
//!
//! shortcut: a changed subresource is detiled whole, not just its blocks in
//! the pages written, and every subresource is still copied into the host
//! texture. Only textures read through descriptor tables are brought up to date
//! (directly indexed views are not known per draw); plane 0 is uploaded
//! (every mip and array slice), depth and stencil planes are not.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::ops::Range;
use std::sync::{Arc, Mutex};

use victory_formats::texture::{Error, Layout};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::*;

use crate::host::{HostList, host};
use crate::object::This;

macro_rules! once {
    ($($arg:tt)*) => {{
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::warn!($($arg)*));
    }};
}

/// Query the primary discrete GPU's dedicated video memory in bytes via DXGI.
fn query_adapter_vram_bytes() -> Option<u64> {
    // SAFETY: plain DXGI adapter queries to inspect video memory.
    unsafe {
        let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).ok()?;
        let mut max_vram = 0u64;
        let mut i = 0u32;
        while let Ok(adapter) = factory.EnumAdapters1(i) {
            i += 1;
            if let Ok(desc) = adapter.GetDesc1()
                && (desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 == 0)
            {
                max_vram = max_vram.max(desc.DedicatedVideoMemory as u64);
            }
        }
        (max_vram > 0).then_some(max_vram)
    }
}

/// The number of most detailed mips to drop for sampled host textures.
/// 0: keep original resolution.
/// 1: drop 1 mip (halves dimensions, ~75% VRAM savings).
/// 2: drop 2 mips (quarters dimensions, ~93.75% VRAM savings).
/// 3: drop 3 mips (eighths dimensions, ~98.4% VRAM savings).
///
/// When `VICTORY_TEXTURE_MIP_DROP` is not explicitly set in the environment,
/// this automatically defaults to 1 on adapters with <= 8 GB dedicated VRAM
/// to prevent host driver committed out-of-memory crashes on 6 GB and 8 GB GPUs.
pub(crate) fn texture_mip_drop() -> u32 {
    static DROP: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *DROP.get_or_init(|| {
        if let Some(v) = std::env::var("VICTORY_TEXTURE_MIP_DROP")
            .ok()
            .and_then(|val| val.parse::<u32>().ok())
        {
            return v.min(3);
        }
        // Auto-detect based on adapter DedicatedVideoMemory.
        // Threshold: 8.5 GiB comfortably covers 4 GB, 6 GB, and 8 GB GPUs
        // (which report up to ~8192 MiB), while leaving >= 10 GB GPUs at full resolution (0).
        if let Some(vram) = query_adapter_vram_bytes() {
            const EIGHT_GB_THRESHOLD: u64 = 8 * 1024 * 1024 * 1024 + 512 * 1024 * 1024;
            if vram <= EIGHT_GB_THRESHOLD {
                tracing::info!(
                    vram_mb = vram / (1024 * 1024),
                    "auto-enabling VICTORY_TEXTURE_MIP_DROP=1 for <= 8 GB GPU"
                );
                return 1;
            }
        }
        0
    })
}

/// The minimum texture dimension (both width and height) eligible for mip-dropping.
/// Defaults to 2048 (the title's sampled 1920x1080 loading artwork turned flat
/// colour below that). Textures smaller than this on either axis are preserved
/// at full resolution. Configurable via `VICTORY_TEXTURE_MIP_DROP_MIN_SIZE`.
pub(crate) fn texture_mip_drop_min_size() -> u64 {
    static MIN_SIZE: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *MIN_SIZE.get_or_init(|| {
        std::env::var("VICTORY_TEXTURE_MIP_DROP_MIN_SIZE")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|&v| v > 0)
            .unwrap_or(2048)
    })
}

/// A texture's upload state: the staging buffer laid out as the host
/// texture's subresources, and what was last detiled.
pub(crate) struct Staging {
    buffer: ID3D12Resource,
    /// Its bytes.
    size: usize,
    data: *mut u8,
    footprints: Vec<UploadFootprint>,
    layout: Layout,
    /// Mips per array slice.
    mips: u32,
    /// Per subresource, the byte ranges of the memory it is detiled from.
    extents: Vec<Vec<Range<usize>>>,
    /// Dropped mips count (offset into console mips for detiling).
    pub(crate) mip_drop: u32,
    memory: *const u8,
    /// Original packed 16-bit format (86 or 115) when the host texture was
    /// widened to R8G8B8A8_UNORM, so detiled 2-byte texels expand into the
    /// 4-byte staging buffer.
    widened_from: Option<u32>,
    last: Mutex<Last>,
    /// Incremented at each detiling; host textures note the one they got
    /// ([`crate::resource::HostTexture::staged`]). Starts above their 0:
    /// the first use always copies.
    generation: AtomicU64,
}

struct UploadFootprint {
    placed: D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
    range: Range<usize>,
    depth_pitch: usize,
}

impl UploadFootprint {
    fn new(
        placed: D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
        rows: u32,
        row_size: u64,
        total: usize,
    ) -> Option<Self> {
        let pitch = placed.Footprint.RowPitch as usize;
        let row_size = usize::try_from(row_size).ok()?;
        let (len, depth_pitch) = footprint_size(pitch, rows, placed.Footprint.Depth, row_size)?;
        let start = usize::try_from(placed.Offset).ok()?;
        let end = start.checked_add(len)?;
        (end <= total).then_some(Self {
            placed,
            range: start..end,
            depth_pitch,
        })
    }
}

/// What a texture's staging buffer holds: the hash of the memory detiled,
/// and the write-watch asking it was last checked after (watched memory),
/// with room to read the memory into.
#[derive(Default)]
struct Last {
    hash: Option<blake3::Hash>,
    asked: u64,
    read: Vec<u8>,
    /// Hashed memory: the presented frame and runtime-write count it was
    /// last read at ([`refresh_now`] reads it again only when one moved).
    ///
    /// [`refresh_now`]: Staging::refresh_now
    checked: Option<(u64, u64)>,
}

/// Title memory writes the runtime made at submission (title copies), after
/// which hashed textures are read again within the frame.
static RUNTIME_WRITES: AtomicU64 = AtomicU64::new(0);

/// The runtime is about to write title memory (a title copy at submission).
pub(crate) fn runtime_writes_title() {
    RUNTIME_WRITES.fetch_add(1, Relaxed);
}

/// Whether any of `changed` overlaps any of `extents` (both sorted).
fn overlaps(extents: &[Range<usize>], changed: &[Range<usize>]) -> bool {
    let (mut i, mut j) = (0, 0);
    while i < extents.len() && j < changed.len() {
        let (e, c) = (&extents[i], &changed[j]);
        if e.start < c.end && c.start < e.end {
            return true;
        }
        if e.end <= c.end {
            i += 1;
        } else {
            j += 1;
        }
    }
    false
}

// SAFETY: the staging mapping is written only at submission, under
// `last`'s lock; the title memory is only read.
unsafe impl Send for Staging {}
// SAFETY: as above.
unsafe impl Sync for Staging {}

static STAGING: Mutex<Option<HashMap<usize, Arc<Staging>>>> = Mutex::new(None);

thread_local! {
    /// Texture memory read for hashing and detiling: one per submitting
    /// thread, as large as the largest texture it read.
    static SCRATCH: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// Textures the host GPU wrote by a copy or resolve: their contents are on
/// the host, not in title memory, which stays as it was.
static HOST_WRITTEN: Mutex<Option<std::collections::HashSet<usize>>> = Mutex::new(None);

/// A host copy or resolve writes `resource`.
pub(crate) fn host_written(resource: This) {
    let mut set = HOST_WRITTEN.lock().unwrap_or_else(|e| e.into_inner());
    set.get_or_insert_with(Default::default)
        .insert(resource as usize);
}

/// The title released a resource.
pub(crate) fn forget(resource: usize) {
    let mut map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(map) = map.as_mut() {
        map.remove(&resource);
    }
    let mut set = HOST_WRITTEN.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(set) = set.as_mut() {
        set.remove(&resource);
    }
}

/// Whether `resource`'s host copy takes its contents from title memory:
/// a texture the host GPU does not render to or write.
pub(crate) fn title_written(texture: &ID3D12Resource) -> bool {
    // SAFETY: a live resource.
    title_written_flags(unsafe { texture.GetDesc() }.Flags)
}

fn title_written_flags(flags: D3D12_RESOURCE_FLAGS) -> bool {
    let host_writable = D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET
        | D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL
        | D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS;
    (flags & host_writable) == D3D12_RESOURCE_FLAG_NONE
}

/// Brings `resource`'s host copy up to date when a list that samples it is
/// submitted: the staging buffer is refreshed (detiled if the memory
/// changed), and the list's prelude copies it into the host texture unless
/// the host texture already holds those bytes. Once per list.
pub(crate) fn upload(list: &HostList, resource: This, texture: &ID3D12Resource) {
    let host_written = HOST_WRITTEN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(|set| set.contains(&(resource as usize)));
    // shortcut: a texture the host copied into keeps the host's contents
    // even if the title later writes its memory too.
    if host_written || !list.first_use(resource as usize) {
        return;
    }
    let (Some(staging), Some(copy)) = (staging(resource, texture), crate::resource::host_copy(resource))
    else {
        return;
    };
    list.defer_texture(staging, copy);
}

/// Whether a host texture that last got generation `held` of its staging
/// buffer must get `generation` (and notes that it does).
fn due(held: &AtomicU64, generation: u64) -> bool {
    held.swap(generation, Relaxed) != generation
}

impl Staging {
    /// At submission, after the refresh: whether the host texture whose
    /// staging generation is `held` lacks the staging buffer's bytes (and
    /// notes that it gets them). Unchanged textures are not copied again
    /// in every list and submission that samples them.
    pub(crate) fn upload_due(&self, held: &AtomicU64) -> bool {
        let due = due(held, self.generation.load(Relaxed));
        #[cfg(feature = "diagnostics")]
        {
            use crate::copystat::{C, add};
            let (n, bytes) = if due {
                (C::TexUploads, C::TexUploadBytes)
            } else {
                (C::TexSkips, C::TexSkipBytes)
            };
            add(n, 1);
            add(bytes, self.size as u64);
        }
        due
    }

    /// Records the copy of the staging buffer into `texture`, which is in
    /// the copy destination state.
    pub(crate) fn record_upload(&self, list: &ID3D12GraphicsCommandList, texture: &ID3D12Resource) {
        for (i, footprint) in self.footprints.iter().enumerate() {
            let to = D3D12_TEXTURE_COPY_LOCATION {
                // SAFETY: a borrowed pointer the location never releases.
                pResource: unsafe { std::mem::transmute_copy(texture) },
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    SubresourceIndex: i as u32,
                },
            };
            let from = D3D12_TEXTURE_COPY_LOCATION {
                // SAFETY: as above.
                pResource: unsafe { std::mem::transmute_copy(&self.buffer) },
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: footprint.placed,
                },
            };
            // SAFETY: an open list; both locations name live resources.
            unsafe { list.CopyTextureRegion(&to, 0, 0, 0, &from, None) };
        }
    }

    pub(crate) fn title_range(&self) -> (usize, usize) {
        (self.memory as usize, self.layout.size as usize)
    }

    /// Detiles the texture memory into the staging buffer if it changed
    /// since the last time (and is mapped at all).
    pub(crate) fn refresh(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        let started = std::time::Instant::now();
        let read = self.refresh_now();
        let preparing = &crate::host::PREPARING;
        preparing[2].fetch_add(started.elapsed().as_micros() as u64, Relaxed);
        preparing[3].fetch_add(read as u64, Relaxed);
    }

    /// Returns the bytes of title memory read.
    fn refresh_now(&self) -> usize {
        let Ok(size) = usize::try_from(self.layout.size) else {
            return 0;
        };
        let start = self.memory as usize;
        let Some(end) = start
            .checked_add(size)
            .filter(|_| size <= isize::MAX as usize)
        else {
            return 0;
        };
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        let last = &mut *last;
        if let Some((now, written)) = crate::watch::writes(start, end, false) {
            let before = last.asked;
            if before != 0 && written.iter().all(|&(_, w)| w <= before) {
                return 0;
            }
            let since = std::mem::replace(&mut last.asked, now);
            let mut read = std::mem::take(&mut last.read);
            if since != 0 && read.len() == size {
                let n = self.changed_pages(last, &mut read, &written, since);
                last.read = read;
                return n;
            }
            // Read through the copy, which stands unmapped pages: memory of
            // big pools is too slow to ask page by page.
            read.resize(size, 0);
            crate::host::copy_mapped(read.as_mut_ptr(), self.memory, size, false);
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::TexWatched, size as u64);
            // The watch is per page and the title writes next to its textures
            // (and rewrites what it wrote): detile only if the bytes changed.
            let n = self.changed(last, &read);
            last.read = read;
            return n;
        }
        if !committed(self.memory, size) {
            #[cfg(feature = "diagnostics")]
            tracing::debug!(
                memory = ?self.memory,
                size,
                desc = ?self.layout.desc,
                "a sampled texture's memory is not committed"
            );
            return 0;
        }
        // Hashing reads the whole texture: once per presented frame (a
        // world frame spans ~17 submissions that each checked it), unless the
        // runtime wrote title memory since.
        // shortcut: a title CPU write into a hashed texture between two
        // submissions of one frame shows from the next frame; if a frame
        // draws such a texture stale, key this on the submission instead.
        let now = (crate::queue::presented(), RUNTIME_WRITES.load(Relaxed));
        if last.hash.is_some() && last.checked == Some(now) {
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::TexFrameSkips, 1);
            return 0;
        }
        last.checked = Some(now);
        // Read through the kernel, as above: the title may unmap the pages
        // (evicting a texture a big table lists) while we read them, and a
        // direct read would fault. The copy is reused, not kept per texture.
        SCRATCH.with_borrow_mut(|tiled| {
            tiled.resize(size, 0);
            crate::host::copy_mapped(tiled.as_mut_ptr(), self.memory, size, false);
            self.changed(last, tiled)
        })
    }

    /// Watched memory read whole before into `read`: reads only the pages
    /// written after asking `since` (by runs of pages), and detiles if any
    /// of their bytes changed; the rest is as it was read. Returns the
    /// bytes read.
    fn changed_pages(
        &self,
        last: &mut Last,
        read: &mut [u8],
        written: &[(usize, u64)],
        since: u64,
    ) -> usize {
        let start = self.memory as usize;
        let (mut bytes, mut changed) = (0, Vec::new());
        for (from, to) in written_runs(written, since, start, start + read.len()) {
            SCRATCH.with_borrow_mut(|now| {
                now.resize(to - from, 0);
                let at = (start + from) as *const u8;
                crate::host::copy_mapped(now.as_mut_ptr(), at, to - from, false);
                if read[from..to] != now[..] {
                    read[from..to].copy_from_slice(now);
                    changed.push(from..to);
                }
            });
            bytes += to - from;
        }
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::TexWatched, bytes as u64);
        if !changed.is_empty() {
            // The hash is of an older read: the next whole read detiles.
            last.hash = None;
            // Only the subresources the changed runs (sorted) reach.
            self.detile(read, Some(&changed));
            self.generation.fetch_add(1, Relaxed);
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::TexChanged, read.len() as u64);
        }
        bytes
    }

    /// After reading the texture's memory into `tiled`: detiles it unless
    /// its hash is the last one's. Returns the bytes read.
    fn changed(&self, last: &mut Last, tiled: &[u8]) -> usize {
        let size = tiled.len();
        let hash = blake3::hash(tiled);
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::TexHashed, size as u64);
        if last.hash == Some(hash) {
            return size;
        }
        last.hash = Some(hash);
        self.detile(tiled, None);
        self.generation.fetch_add(1, Relaxed);
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::TexChanged, size as u64);
        size
    }

    /// Detiles `tiled` (the texture's memory) into the staging buffer: the
    /// subresources whose memory overlaps `changed` (sorted byte ranges),
    /// or all of them.
    fn detile(&self, tiled: &[u8], changed: Option<&[Range<usize>]>) {
        #[cfg(feature = "diagnostics")]
        if tiled.iter().all(|&b| b == 0) {
            tracing::debug!(
                memory = ?self.memory,
                desc = ?self.layout.desc,
                "a sampled texture's memory is all zero (not streamed yet, or a failed BCPack read)"
            );
        }
        // SAFETY: the upload buffer is mapped for exactly this range and
        // refresh holds `last`'s lock while writing it.
        let linear = unsafe { std::slice::from_raw_parts_mut(self.data, self.size) };
        let skip = |i: usize| changed.is_some_and(|c| !overlaps(&self.extents[i], c));
        if let Err((e, console_mip, slice)) = detile_plane(
            &self.layout,
            self.mips,
            self.mip_drop,
            &self.footprints,
            self.widened_from,
            tiled,
            linear,
            skip,
        ) {
            tracing::warn!(?e, console_mip, slice, "texture not detiled");
        }
    }
}

/// Detiles plane 0's host subresources (but those `skip` names) from
/// `tiled` into `linear`, the staging buffer laid out by `footprints`.
/// Errors name the console mip and the slice.
#[allow(clippy::too_many_arguments)]
fn detile_plane(
    layout: &Layout,
    mips: u32,
    mip_drop: u32,
    footprints: &[UploadFootprint],
    widened_from: Option<u32>,
    tiled: &[u8],
    linear: &mut [u8],
    skip: impl Fn(usize) -> bool,
) -> Result<(), (Error, u32, u32)> {
    let mut packed = Vec::new();
    for (i, footprint) in footprints.iter().enumerate() {
        if skip(i) {
            continue;
        }
        // Host subresources skip the dropped mips of the title's layout.
        let (mip, slice) = (i as u32 % mips + mip_drop, i as u32 / mips);
        let f = footprint.placed.Footprint;
        let pitch = f.RowPitch as usize;
        // The range ends at the last row's data bytes, as D3D12 allocates.
        let target = linear
            .get_mut(footprint.range.clone())
            .ok_or((Error::TooSmall, mip, slice))?;
        detile_subresource(
            layout,
            tiled,
            mip,
            slice,
            target,
            pitch,
            footprint.depth_pitch / pitch,
            f.Depth as usize,
            widened_from,
            &mut packed,
        )
        .map_err(|e| (e, mip, slice))?;
    }
    Ok(())
}

/// Bytes read by a copy and the stride between depth slices.
fn footprint_size(pitch: usize, rows: u32, depth: u32, row_size: usize) -> Option<(usize, usize)> {
    if row_size == 0 || row_size > pitch {
        return None;
    }
    let depth_pitch = pitch.checked_mul(rows as usize)?;
    // D3D12 allocates the data bytes of the last row only.
    let preceding_rows = (rows as usize)
        .checked_mul(depth as usize)?
        .checked_sub(1)?;
    let len = pitch.checked_mul(preceding_rows)?.checked_add(row_size)?;
    Some((len, depth_pitch))
}

/// The runs of whole pages written after asking `since` (`written`: pages
/// and their last writes, in page order), cut to `[start, end)`, as byte
/// ranges from `start`.
fn written_runs(
    written: &[(usize, u64)],
    since: u64,
    start: usize,
    end: usize,
) -> Vec<(usize, usize)> {
    use crate::watch::PAGE;
    let mut runs: Vec<(usize, usize)> = Vec::new();
    for &(page, _) in written.iter().filter(|&&(_, w)| w > since) {
        let (from, to) = ((page * PAGE).max(start), ((page + 1) * PAGE).min(end));
        if from >= to {
            continue;
        }
        match runs.last_mut() {
            Some((_, last)) if *last == from - start => *last = to - start,
            _ => runs.push((from - start, to - start)),
        }
    }
    runs
}

#[allow(clippy::too_many_arguments)]
fn detile_subresource(
    layout: &Layout,
    tiled: &[u8],
    mip: u32,
    slice: u32,
    linear: &mut [u8],
    row_pitch: usize,
    rows: usize,
    depth: usize,
    widened_from: Option<u32>,
    packed: &mut Vec<u8>,
) -> Result<(), Error> {
    let Some(packed_format) = widened_from else {
        return layout.detile(tiled, mip, slice, linear, row_pitch, row_pitch * rows);
    };
    let width = layout
        .mips
        .get(mip as usize)
        .map_or(0, |m| m.width as usize);
    let packed_pitch = width * 2;
    let packed_plane = packed_pitch * rows;
    packed.resize(packed_plane * depth, 0);
    layout.detile(tiled, mip, slice, packed, packed_pitch, packed_plane)?;
    crate::resource::expand_packed16_rows(
        packed_format,
        width,
        rows * depth,
        packed,
        packed_pitch,
        linear,
        row_pitch,
    );
    Ok(())
}

/// Whether `[at, at + len)` is committed memory (streamed textures may be
/// unmapped until their pages arrive).
pub(crate) fn committed(at: *const u8, len: usize) -> bool {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEMORY_BASIC_INFORMATION, VirtualQuery};
    let mut p = at as usize;
    let Some(end) = p.checked_add(len) else {
        return false;
    };
    while p < end {
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        let n = unsafe { VirtualQuery(p as *const _, &mut info, size_of_val(&info)) };
        if n == 0 || info.State != MEM_COMMIT {
            return false;
        }
        let Some(next) = (info.BaseAddress as usize).checked_add(info.RegionSize) else {
            return false;
        };
        p = next;
    }
    true
}

fn plane_subresources(desc: &D3D12_RESOURCE_DESC) -> u32 {
    let mips = u32::from(desc.MipLevels);
    if desc.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D {
        mips
    } else {
        mips * u32::from(desc.DepthOrArraySize)
    }
}

/// Committed regions already found, for checking many addresses at once:
/// one `VirtualQuery` per region instead of one per address (a query walks
/// its region's pages to size it, ~90 µs in a multi-GB title region).
#[derive(Default)]
pub(crate) struct CommittedRegions(std::collections::BTreeMap<usize, usize>);

impl CommittedRegions {
    /// Whether all of `[at, at + len)` is committed, as [`committed`].
    pub(crate) fn has(&mut self, at: usize, len: usize) -> bool {
        use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEMORY_BASIC_INFORMATION, VirtualQuery};
        let end = at.saturating_add(len);
        let mut p = at;
        while p < end {
            if let Some((_, &region_end)) = self.0.range(..=p).next_back()
                && region_end > p
            {
                p = region_end;
                continue;
            }
            // SAFETY: plain data filled in by the query.
            let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
            // SAFETY: a valid out pointer of the right size.
            let n = unsafe { VirtualQuery(p as *const _, &mut info, size_of_val(&info)) };
            if n == 0 || info.State != MEM_COMMIT {
                return false;
            }
            let base = info.BaseAddress as usize;
            self.0.insert(base, base + info.RegionSize);
            p = base + info.RegionSize;
        }
        true
    }
}

/// The staging state of a title texture, made on first use.
fn staging(resource: This, texture: &ID3D12Resource) -> Option<Arc<Staging>> {
    let mut map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    let map = map.get_or_insert_with(HashMap::new);
    if let Some(s) = map.get(&(resource as usize)) {
        return Some(s.clone());
    }
    let (memory, desc) = crate::resource::texture_memory(resource)?;
    let Some(layout) = crate::resource::texture_layout(&desc).map(|t| t.planes[0].layout.clone())
    else {
        once!(
            ?desc,
            "texture contents without a console layout not uploaded"
        );
        return None;
    };
    if layout.desc.swizzle.xor() {
        once!(?desc, "texture contents in an XOR mode not uploaded");
        return None;
    }
    let host = host()?;
    // SAFETY: a live resource.
    let d = unsafe { texture.GetDesc() };
    let title_format = crate::resource::pc_format(desc.format);
    let widened_from = (d.Format.0 as u32 == crate::resource::RGBA8_UNORM
        && crate::resource::PACKED_16.contains(&title_format))
    .then_some(title_format);
    let mip_drop = u32::from(desc.mip_levels).saturating_sub(u32::from(d.MipLevels));
    let count = plane_subresources(&d);
    if count == 0 {
        return None;
    }
    let mut footprints = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); count as usize];
    let mut rows = vec![0u32; count as usize];
    let mut row_sizes = vec![0u64; count as usize];
    let mut total = 0;
    // SAFETY: outputs for `count` subresources.
    unsafe {
        host.device.GetCopyableFootprints(
            &d,
            0,
            count,
            0,
            Some(footprints.as_mut_ptr()),
            Some(rows.as_mut_ptr()),
            Some(row_sizes.as_mut_ptr()),
            Some(&mut total),
        );
    }
    let size = usize::try_from(total)
        .ok()
        .filter(|&size| size <= isize::MAX as usize)?;
    let footprints = footprints
        .into_iter()
        .zip(rows)
        .zip(row_sizes)
        .map(|((placed, rows), row_size)| UploadFootprint::new(placed, rows, row_size, size))
        .collect::<Option<Vec<_>>>()?;
    let (buffer, data) = host.upload_buffer(total)?;
    crate::host::name(&buffer, || {
        format!(
            "staging of {resource:?} {}x{}x{} f{}",
            d.Width, d.Height, d.DepthOrArraySize, d.Format.0
        )
    });
    tracing::debug!(?desc, total, mip_drop, "texture contents uploaded from title memory");
    let mips = u32::from(d.MipLevels).max(1);
    let whole: Vec<_> = std::iter::once(0..layout.size as usize).collect();
    let extents = (0..count)
        // Host subresources skip the dropped mips of the title's layout.
        .map(|i| match layout.extents(i % mips + mip_drop, i / mips) {
            Ok(e) => e
                .into_iter()
                .map(|r| r.start as usize..r.end as usize)
                .collect(),
            // Detiling it fails too; any change "touches" it.
            Err(_) => whole.clone(),
        })
        .collect();
    let s = Arc::new(Staging {
        buffer,
        size,
        data,
        footprints,
        layout,
        mips: u32::from(d.MipLevels),
        extents,
        mip_drop,
        memory,
        widened_from,
        last: Mutex::default(),
        generation: AtomicU64::new(1),
    });
    map.insert(resource as usize, s.clone());
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use victory_formats::texture::{Dimension, Swizzle, Texture, TextureDesc};
    use windows::Win32::Graphics::Dxgi::Common::{
        DXGI_FORMAT_BC1_UNORM, DXGI_FORMAT_BC7_UNORM, DXGI_FORMAT_R8_UNORM,
        DXGI_FORMAT_R8G8B8A8_UNORM,
    };

    #[test]
    fn adapter_vram_query_does_not_panic() {
        if let Some(bytes) = query_adapter_vram_bytes() {
            assert!(bytes > 0);
        }
    }

    #[test]
    fn ranges_overlap_only_when_they_share_a_byte() {
        let extents = [10..20, 40..50];
        assert!(overlaps(&extents, &[15..16]));
        assert!(overlaps(&extents, &[0..5, 49..60]));
        assert!(overlaps(&extents, &[0..11]));
        // Touching ends share no byte.
        assert!(!overlaps(&extents, &[20..40]));
        assert!(!overlaps(&extents, &[0..10, 20..40, 50..90]));
        assert!(!overlaps(&extents, &[]));
        assert!(!overlaps(&[], &[0..100]));
        // A long change across several extents, and the reverse.
        assert!(overlaps(&[10..20, 40..50], &[0..100]));
        assert!(overlaps(&[0..100], &[10..20, 40..50]));
    }

    #[test]
    fn committed_regions_answer_as_a_query_and_remember_the_region() {
        let data = vec![1u8; 1 << 20];
        let mut regions = CommittedRegions::default();
        let at = data.as_ptr() as usize;
        assert!(regions.has(at, data.len()));
        assert!(!regions.0.is_empty());
        assert!(regions.has(at + 4096, 1), "a later address in a known region");
        assert!(!regions.has(0x10, 1), "the null page is never committed");
        assert_eq!(regions.has(at, data.len()), committed(data.as_ptr(), data.len()));
    }

    #[test]
    fn written_runs_join_neighbouring_pages_and_cut_to_the_memory() {
        use crate::watch::PAGE;
        let written = [(10, 3), (11, 5), (12, 6), (14, 2), (15, 9), (20, 7)];
        let (start, end) = (10 * PAGE + 16, 16 * PAGE - 32);
        assert_eq!(
            written_runs(&written, 4, start, end),
            [(PAGE - 16, 3 * PAGE - 16), (5 * PAGE - 16, end - start)]
        );
        assert!(written_runs(&written, 9, start, end).is_empty());
    }

    #[test]
    fn unchanged_staging_is_copied_once() {
        // A new host texture (0) gets the staging buffer's first contents.
        let held = AtomicU64::new(0);
        assert!(due(&held, 1));
        // Later lists and submissions with the same bytes copy nothing ...
        assert!(!due(&held, 1));
        assert!(!due(&held, 1));
        // ... until a detiling changes them.
        assert!(due(&held, 2));
        assert!(!due(&held, 2));
        // A host texture made again starts over.
        assert!(due(&AtomicU64::new(0), 2));
    }

    #[test]
    fn detile_widened_packed16_expands_into_rgba8_staging() {
        for format in crate::resource::PACKED_16 {
            let desc = TextureDesc {
                dimension: Dimension::Tex2d,
                width: 17,
                height: 9,
                depth_or_array: 1,
                mips: 2,
                format,
                samples: 1,
                swizzle: Swizzle::SW_4KB_S,
            };
            let tex = Texture::new(&desc).unwrap();
            let layout = &tex.planes[0].layout;
            let mut tiled = vec![0u8; layout.size as usize];
            let mut expected_mips = Vec::new();

            for mip in 0..2u32 {
                let m = layout.mips[mip as usize];
                let row_bytes = m.width as usize * 2;
                let mut src_mip = vec![0u8; row_bytes * m.height as usize];
                for y in 0..m.height as usize {
                    for x in 0..m.width as usize {
                        let px = ((mip as u16 + 1) * 0x1111)
                            .wrapping_add((y as u16) * 0x0103 + x as u16);
                        let at = y * row_bytes + x * 2;
                        src_mip[at..at + 2].copy_from_slice(&px.to_le_bytes());
                    }
                }
                layout
                    .tile(&mut tiled, mip, 0, &src_mip, row_bytes, src_mip.len())
                    .unwrap();
                expected_mips.push(src_mip);
            }

            let mut packed = Vec::new();
            for (mip, src_mip) in expected_mips.iter().enumerate() {
                let m = layout.mips[mip];
                let width = m.width as usize;
                let rows = m.height as usize;
                let row_pitch = (width * 4).next_multiple_of(256);
                let exact_len = row_pitch * (rows - 1) + width * 4;
                let mut staging = vec![0x5au8; exact_len];
                detile_subresource(
                    layout,
                    &tiled,
                    mip as u32,
                    0,
                    &mut staging,
                    row_pitch,
                    rows,
                    1,
                    Some(format),
                    &mut packed,
                )
                .unwrap();
                for y in 0..rows {
                    let mut expected = vec![0u8; width * 4];
                    crate::resource::expand_packed16(
                        format,
                        &src_mip[y * width * 2..][..width * 2],
                        &mut expected,
                    );
                    assert_eq!(
                        &staging[y * row_pitch..][..width * 4],
                        &expected[..],
                        "format {format} mip {mip} row {y}"
                    );
                }
            }
        }
    }

    #[test]
    fn footprint_ends_after_the_last_rows_data() {
        // Two depth slices of three rows. The sixth row starts at 1280,
        // but only its 20 data bytes belong to the upload range.
        assert_eq!(footprint_size(256, 3, 2, 20), Some((1300, 768)));
    }

    #[test]
    fn footprint_rejects_invalid_geometry_and_out_of_bounds_placement() {
        assert_eq!(footprint_size(256, 0, 1, 20), None);
        assert_eq!(footprint_size(256, 1, 0, 20), None);
        assert_eq!(footprint_size(256, 1, 1, 0), None);
        assert_eq!(footprint_size(256, 1, 1, 257), None);
        assert_eq!(footprint_size(usize::MAX, 1, 2, 1), None);

        let placed = placed(512, [5, 3, 2]);
        let footprint = UploadFootprint::new(placed, 3, 20, 1812).unwrap();
        assert_eq!(footprint.range, 512..1812);
        assert!(UploadFootprint::new(placed, 3, 20, 1811).is_none());
        let overflowing = D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
            Offset: u64::MAX,
            ..placed
        };
        assert!(UploadFootprint::new(overflowing, 3, 20, usize::MAX).is_none());
    }

    #[test]
    fn plane_zero_counts_each_array_mip_and_each_volume_mip_once() {
        let mut desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE1D,
            DepthOrArraySize: 6,
            MipLevels: 4,
            ..Default::default()
        };
        assert_eq!(plane_subresources(&desc), 24);
        desc.Dimension = D3D12_RESOURCE_DIMENSION_TEXTURE2D;
        assert_eq!(plane_subresources(&desc), 24);
        desc.Dimension = D3D12_RESOURCE_DIMENSION_TEXTURE3D;
        assert_eq!(plane_subresources(&desc), 4);
    }

    fn placed(offset: u64, [width, height, depth]: [u32; 3]) -> D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
        D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
            Offset: offset,
            Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                Width: width,
                Height: height,
                Depth: depth,
                RowPitch: 256,
                ..Default::default()
            },
        }
    }

    // Guest mip and slice, host offset and dimensions, then rows and bytes per row.
    type Shape = ((u32, u32), u64, [u32; 3], u32, u64);

    fn assert_detiles(desc: TextureDesc, element_bytes: usize, shapes: &[Shape], total: usize) {
        let layout = &Texture::new(&desc).unwrap().planes[0].layout;
        let mut tiled = vec![0u8; layout.size as usize];
        let mut linear = vec![0xff; total];
        let mut expected = linear.clone();
        let mut footprints = Vec::new();
        for (i, &((mip, slice), offset, dims, rows, row_size)) in shapes.iter().enumerate() {
            footprints
                .push(UploadFootprint::new(placed(offset, dims), rows, row_size, total).unwrap());
            for z in 0..dims[2] {
                for y in 0..rows {
                    for x in 0..row_size as usize / element_bytes {
                        let guest = layout.offset(mip, x as u32, y, slice + z).unwrap() as usize;
                        let host =
                            offset as usize + (z * rows + y) as usize * 256 + x * element_bytes;
                        let value = (i * 37 + (z * 7 + y * 13) as usize + x * 3) as u8;
                        for b in 0..element_bytes {
                            tiled[guest + b] = value + b as u8;
                            expected[host + b] = value + b as u8;
                        }
                    }
                }
            }
        }
        detile_plane(layout, desc.mips, 0, &footprints, None, &tiled, &mut linear, |_| false)
            .unwrap();
        assert_eq!(linear, expected);
    }

    #[test]
    fn array_upload_uses_mip_first_order_and_preserves_padding() {
        let desc = TextureDesc {
            dimension: Dimension::Tex2d,
            width: 7,
            height: 5,
            depth_or_array: 2,
            mips: 2,
            format: DXGI_FORMAT_R8G8B8A8_UNORM.0 as u32,
            samples: 1,
            swizzle: Swizzle::SW_4KB_S,
        };
        assert_detiles(
            desc,
            4,
            &[
                ((0, 0), 0, [7, 5, 1], 5, 28),
                ((1, 0), 1536, [3, 2, 1], 2, 12),
                ((0, 1), 2048, [7, 5, 1], 5, 28),
                ((1, 1), 3584, [3, 2, 1], 2, 12),
            ],
            3852,
        );
    }

    #[test]
    fn volume_upload_uses_pitched_depth_slices_and_shrinks_depth_by_mip() {
        let desc = TextureDesc {
            dimension: Dimension::Tex3d,
            width: 5,
            height: 3,
            depth_or_array: 3,
            mips: 2,
            format: DXGI_FORMAT_R8_UNORM.0 as u32,
            samples: 1,
            swizzle: Swizzle::LINEAR,
        };
        assert_detiles(
            desc,
            1,
            &[
                ((0, 0), 0, [5, 3, 3], 3, 5),
                ((1, 0), 2560, [2, 1, 1], 1, 2),
            ],
            2562,
        );
    }

    #[test]
    fn block_compressed_upload_uses_whole_blocks_for_partial_rows_and_mips() {
        for (format, block_bytes) in [(DXGI_FORMAT_BC1_UNORM, 8), (DXGI_FORMAT_BC7_UNORM, 16)] {
            let desc = TextureDesc {
                dimension: Dimension::Tex2d,
                width: 7,
                height: 5,
                depth_or_array: 1,
                mips: 2,
                format: format.0 as u32,
                samples: 1,
                swizzle: Swizzle::SW_4KB_S,
            };
            assert_detiles(
                desc,
                block_bytes as usize,
                &[
                    ((0, 0), 0, [8, 8, 1], 2, block_bytes * 2),
                    ((1, 0), 512, [4, 4, 1], 1, block_bytes),
                ],
                512 + block_bytes as usize,
            );
        }
    }

    #[test]
    fn host_writable_flags_exclude_texture_uploads() {
        assert!(title_written_flags(D3D12_RESOURCE_FLAG_NONE));
        assert!(title_written_flags(
            D3D12_RESOURCE_FLAG_DENY_SHADER_RESOURCE
        ));
        for flag in [
            D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
            D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL,
            D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        ] {
            assert!(!title_written_flags(flag));
            assert!(!title_written_flags(
                flag | D3D12_RESOURCE_FLAG_DENY_SHADER_RESOURCE
            ));
        }
    }

    #[test]
    fn committed_rejects_an_address_range_that_wraps() {
        assert!(!committed(std::ptr::without_provenance(usize::MAX), 2));
        assert!(committed(std::ptr::without_provenance(usize::MAX), 0));
        assert!(!committed(std::ptr::null(), 1));
    }

    #[test]
    fn committed_checks_every_region_in_the_range() {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READONLY, PAGE_READWRITE, VirtualAlloc,
            VirtualFree,
        };

        // SAFETY: reserve two pages, then commit them separately with
        // different protections so VirtualQuery reports distinct regions.
        unsafe {
            let base = VirtualAlloc(std::ptr::null(), 8192, MEM_RESERVE, PAGE_READWRITE);
            assert!(!base.is_null());
            assert_eq!(VirtualAlloc(base, 4096, MEM_COMMIT, PAGE_READWRITE), base);
            let bytes = base.cast::<u8>();
            assert!(committed(bytes, 4096));
            assert!(!committed(bytes.add(4095), 2));
            let second = bytes.add(4096).cast();
            assert_eq!(
                VirtualAlloc(second, 4096, MEM_COMMIT, PAGE_READONLY),
                second
            );
            assert!(committed(bytes.add(4095), 2));
            assert!(committed(bytes, 8192));
            assert_ne!(VirtualFree(base, 0, MEM_RELEASE), 0);
        }
    }
}
