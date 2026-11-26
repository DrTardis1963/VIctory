//! A frametime dump: one CSV row per frame summary (about a second), with the
//! frame time, the buckets `host.rs` already measures (preparing, copies,
//! textures, GPU, copy-back, heap-buffer passes, texture refreshes, the passes
//! over the card's copies), and the worst and average frame over the
//! interval, timed at every present.
//!
//! The app turns it on with [`set_path`] (`VICTORY_FRAMETIME_DUMP=1` puts it at
//! `<cache>\frametime.csv`, any other value is the file), after checking the
//! path is outside the repository. The file is truncated at the first row.
//! Deliberately release-buildable (no `diagnostics` feature): the frames it
//! is meant to explain are the slow ones.
//!
//! The columns match the dumps the community collects with
//! `skornuta/victory-fork`, where this file comes from, so they compare
//! directly.

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

static PATH: OnceLock<PathBuf> = OnceLock::new();
static FILE: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();
static METADATA: OnceLock<String> = OnceLock::new();
static BUILD_LABEL: OnceLock<String> = OnceLock::new();

/// Turns the dump on, writing to `path`. The caller checks the asset policy
/// (`victory_core::ensure_outside_repo`) first. Call before the first frame.
pub fn set_path(path: PathBuf) {
    let _ = PATH.set(path);
}

/// The title build the app identified (`<label> (<hash>)`), for the header.
pub fn set_build_label(label: String) {
    let _ = BUILD_LABEL.set(label);
}

/// When the previous present happened and how long the frames since the
/// last summary took (microseconds, and a count).
static LAST: Mutex<Option<Instant>> = Mutex::new(None);
static MAX_US: AtomicU64 = AtomicU64::new(0);
static SUM_US: AtomicU64 = AtomicU64::new(0);
static COUNT: AtomicU64 = AtomicU64::new(0);
/// Frames presented since the dump started.
static PRESENTED: AtomicU64 = AtomicU64::new(0);

fn enabled() -> bool {
    PATH.get().is_some()
}

fn file() -> Option<&'static Mutex<std::fs::File>> {
    FILE.get_or_init(|| {
        let path = PATH.get()?;
        match std::fs::File::create(path) {
            Ok(mut file) => {
                let _ = file.write_all(metadata().as_bytes());
                let _ = file.write_all(HEADER.as_bytes());
                let _ = file.flush();
                tracing::info!(path = %path.display(), "frametime dump");
                Some(Mutex::new(file))
            }
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "no frametime dump");
                None
            }
        }
    })
    .as_ref()
}

const HEADER: &str = "t_ms,frame,frames,fps,ms_per_frame,frame_ms_max,frame_ms_avg,\
cpu_prepare_ms,gpu_ms,gpu_pct,copies_ms,copies_mb,textures_ms,textures_mb,\
copyback_ms,copyback_mb,draws,dropped,making,indirect,heap_passes,heap_ms,\
texture_refresh_ms,card_passes,card_compares,card_sent_mb,card_ranges,\
card_pieces,card_fetched,card_fetched_mb,card_waits,card_wait_ms,\
twins_into_depth,twins_into_colour,depth_planes,ram_mb,cpu_pct\n";

/// The `#` block a dump starts with: the machine and the switches, so a file
/// from another PC can be compared and reproduced. `pandas.read_csv(...,
/// comment='#')` skips it.
fn metadata() -> &'static str {
    METADATA.get_or_init(|| {
        let mut out = String::from("# victory frametime dump\n");
        out.push_str(&format!("# version: {}\n", env!("CARGO_PKG_VERSION")));
        if let Some(build) = BUILD_LABEL.get() {
            out.push_str(&format!("# game: {build}\n"));
        }
        #[cfg(windows)]
        {
            let cpu = registry_string(
                r"HARDWARE\DESCRIPTION\System\CentralProcessor\0",
                "ProcessorNameString",
            )
            .map(|name| name.trim().to_owned())
            .unwrap_or_else(|| {
                std::env::var("PROCESSOR_IDENTIFIER").unwrap_or_else(|_| "unknown".into())
            });
            let logical = std::thread::available_parallelism().map_or(0, |n| n.get());
            out.push_str(&format!("# cpu: {cpu} ({logical} logical processors)\n"));
            let gpu = crate::host::adapter_summary();
            if let Some((name, vram)) = &gpu {
                out.push_str(&format!("# gpu: {name} ({} MiB dedicated)\n", vram >> 20));
            }
            if let Some(driver) = display_driver_version(gpu.as_ref().map(|(n, _)| n.as_str())) {
                out.push_str(&format!("# driver: {driver}\n"));
            }
            if let Some(total) = total_ram_mib() {
                out.push_str(&format!("# ram: {total} MiB\n"));
            }
            const NT: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";
            let product = registry_string(NT, "ProductName");
            let build = registry_string(NT, "CurrentBuild");
            let release = registry_string(NT, "DisplayVersion");
            // Windows 11 keeps "Windows 10 ..." in `ProductName`.
            let mut product = product.unwrap_or_default();
            if build
                .as_deref()
                .and_then(|b| b.parse::<u32>().ok())
                .is_some_and(|b| b >= 22_000)
            {
                product = product.replace("Windows 10", "Windows 11");
            }
            if !product.is_empty() || build.is_some() {
                out.push_str(&format!(
                    "# os: {product}{}{}\n",
                    release.map(|r| format!(" {r}")).unwrap_or_default(),
                    build.map(|b| format!(" (build {b})")).unwrap_or_default()
                ));
            }
            const BIOS: &str = r"HARDWARE\DESCRIPTION\System\BIOS";
            let board = match (
                registry_string(BIOS, "BaseBoardManufacturer"),
                registry_string(BIOS, "BaseBoardProduct"),
            ) {
                (Some(m), Some(p)) => Some(format!("{m} {p}")),
                (m, p) => m.or(p),
            };
            if let Some(board) = board {
                out.push_str(&format!("# board: {board}\n"));
            }
            use windows::Win32::UI::WindowsAndMessaging::{
                GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN,
            };
            // SAFETY: a plain query of the primary display's size.
            let (w, h) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
            if w > 0 && h > 0 {
                out.push_str(&format!("# display: {w}x{h}\n"));
            }
        }
        out.push_str(&switches_line(std::env::vars()));
        out
    })
}

/// `# switches: ...` for the `VICTORY_*` variables set, sorted; of a value
/// naming a file or folder only its last part, since a dump is made to be
/// posted. Empty when none are set.
fn switches_line(vars: impl Iterator<Item = (String, String)>) -> String {
    let shown = |value: String| match value.rfind(['/', '\\']) {
        Some(at) => format!("...{}", &value[at..]),
        None => value,
    };
    let mut switches: Vec<String> = vars
        .filter(|(key, value)| key.starts_with("VICTORY_") && !value.is_empty())
        .map(|(key, value)| format!("{key}={}", shown(value)))
        .collect();
    if switches.is_empty() {
        return String::new();
    }
    switches.sort();
    format!("# switches: {}\n", switches.join(" "))
}

/// A `REG_SZ` value under `HKLM`, or `None`.
#[cfg(windows)]
fn registry_string(subkey: &str, value: &str) -> Option<String> {
    use windows_sys::Win32::System::Registry::{
        HKEY_LOCAL_MACHINE, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ, RegGetValueW,
    };
    let wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain([0]).collect() };
    let (subkey, value) = (wide(subkey), wide(value));
    let flags = RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ;
    let mut bytes = 0u32;
    // SAFETY: a size query with NUL-terminated names.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            subkey.as_ptr(),
            value.as_ptr(),
            flags,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut bytes,
        )
    };
    if status != 0 || bytes == 0 {
        return None;
    }
    let mut buffer = vec![0u16; (bytes as usize).div_ceil(2)];
    // SAFETY: a read into a buffer of the size the query gave.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            subkey.as_ptr(),
            value.as_ptr(),
            flags,
            std::ptr::null_mut(),
            buffer.as_mut_ptr().cast(),
            &mut bytes,
        )
    };
    if status != 0 {
        return None;
    }
    let len = (bytes as usize / 2).saturating_sub(1).min(buffer.len());
    Some(String::from_utf16_lossy(&buffer[..len]))
}

/// Total physical RAM, in MiB.
#[cfg(windows)]
fn total_ram_mib() -> Option<u64> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    // SAFETY: plain data the call fills in.
    let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    // SAFETY: a sized structure.
    let ok = unsafe { GlobalMemoryStatusEx(&mut m) } != 0;
    ok.then_some(m.ullTotalPhys >> 20)
}

/// The display class key's driver version; for NVIDIA also the public number
/// (`32.0.16.2124` -> `621.24`), which is what people quote. The class has a
/// subkey per display adapter (`0000`, `0001`, ...), so match `DriverDesc`
/// against the DXGI adapter rather than assuming `0000`.
#[cfg(windows)]
fn display_driver_version(gpu: Option<&str>) -> Option<String> {
    const CLASS: &str =
        r"SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}";
    let gpu = gpu?.to_ascii_lowercase();
    let mut fallback = None;
    for index in 0..16 {
        let key = format!(r"{CLASS}\{index:04}");
        let Some(desc) = registry_string(&key, "DriverDesc").filter(|d| !d.is_empty()) else {
            continue;
        };
        let desc = desc.to_ascii_lowercase();
        let nvidia = desc.contains("nvidia");
        if index == 0 {
            fallback = registry_string(&key, "DriverVersion");
        }
        let matches =
            desc.contains(&gpu) || gpu.contains(&desc) || (nvidia && gpu.contains("nvidia"));
        if !matches {
            continue;
        }
        let version = registry_string(&key, "DriverVersion")?;
        return Some(
            match nvidia.then(|| nvidia_driver_number(&version)).flatten() {
                Some(public) => format!("{public} ({version})"),
                None => version,
            },
        );
    }
    fallback
}

/// `32.0.16.2124` -> `621.24`: the last five digits of the third and fourth
/// parts.
fn nvidia_driver_number(windows: &str) -> Option<String> {
    let parts: Vec<&str> = windows.split('.').collect();
    let [_, _, third, fourth] = parts[..] else {
        return None;
    };
    let digits: String = format!("{third}{fourth}")
        .chars()
        .filter(char::is_ascii_digit)
        .collect();
    let tail = digits.get(digits.len().checked_sub(5)?..)?;
    Some(format!("{}.{}", &tail[..3], &tail[3..]))
}

/// Times one presented frame. Called for every present, before the frame
/// summary's own one-second gate, so a slow frame is seen even when the
/// summary is not printed.
pub(crate) fn tick() {
    if !enabled() {
        return;
    }
    PRESENTED.fetch_add(1, Ordering::Relaxed);
    let now = Instant::now();
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(previous) = *last {
        let micros = now.duration_since(previous).as_micros() as u64;
        MAX_US.fetch_max(micros, Ordering::Relaxed);
        SUM_US.fetch_add(micros, Ordering::Relaxed);
        COUNT.fetch_add(1, Ordering::Relaxed);
    }
    *last = Some(now);
}

/// The worst and average frame in the interval, in milliseconds; resets them.
fn take_interval() -> (f64, f64) {
    let max = MAX_US.swap(0, Ordering::Relaxed) as f64 / 1000.0;
    let sum = SUM_US.swap(0, Ordering::Relaxed) as f64 / 1000.0;
    let count = COUNT.swap(0, Ordering::Relaxed);
    (max, if count > 0 { sum / count as f64 } else { 0.0 })
}

/// One summary interval's figures from `host.rs`: times per frame in ms,
/// sizes per frame in MB, counts per frame.
#[derive(Default)]
pub(crate) struct Row {
    pub frames: f64,
    pub fps: f64,
    pub ms: f64,
    pub cpu_prepare_ms: f64,
    pub gpu_ms: f64,
    pub copies_ms: f64,
    pub copies_mb: f64,
    pub textures_ms: f64,
    pub textures_mb: f64,
    pub copyback_ms: f64,
    pub copyback_mb: f64,
    pub draws: f64,
    pub dropped: f64,
    pub making: f64,
    pub indirect: f64,
    pub heap_passes: f64,
    pub heap_ms: f64,
    pub texture_refresh_ms: f64,
    pub card_passes: f64,
    pub card_compares: f64,
    /// `crate::shadow::card_stats()`, totals over the interval.
    pub card: [u64; 10],
    pub twins_into_depth: f64,
    pub twins_into_colour: f64,
    pub depth_planes: f64,
}

/// The process's CPU time (all threads) when the last row was written, and when.
static CPU_LAST: Mutex<Option<(u64, Instant)>> = Mutex::new(None);

/// The process's share of the whole machine's CPU since the last row, in percent.
#[cfg(windows)]
fn cpu_pct() -> f64 {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let mut t = [FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    }; 4];
    let [created, exited, kernel, user] = &mut t;
    // SAFETY: the current-process pseudo handle and four FILETIMEs to fill.
    if unsafe { GetProcessTimes(GetCurrentProcess(), created, exited, kernel, user) } == 0 {
        return 0.0;
    }
    let ticks = |f: &FILETIME| (u64::from(f.dwHighDateTime) << 32) | u64::from(f.dwLowDateTime);
    let busy_us = (ticks(&t[2]) + ticks(&t[3])) / 10;
    let now = Instant::now();
    let mut last = CPU_LAST.lock().unwrap_or_else(|e| e.into_inner());
    let pct = match *last {
        Some((before, at)) => {
            let wall = now.duration_since(at).as_secs_f64();
            let logical = std::thread::available_parallelism().map_or(1, |n| n.get()) as f64;
            (busy_us.saturating_sub(before) as f64 / 1e6) / wall.max(1e-6) / logical * 100.0
        }
        None => 0.0,
    };
    *last = Some((busy_us, now));
    pct
}

/// The process's working set, in MB.
#[cfg(windows)]
fn ram_mb() -> f64 {
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // SAFETY: plain data, zero is valid.
    let mut c: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    // SAFETY: the current-process pseudo handle and a counter of the size passed.
    let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, size) } != 0;
    if ok {
        c.WorkingSetSize as f64 / (1024.0 * 1024.0)
    } else {
        0.0
    }
}

/// Appends `r` when the dump is on.
#[cfg(windows)]
pub(crate) fn record(r: &Row) {
    if !enabled() {
        return;
    }
    let Some(file) = file() else {
        return;
    };
    let (max, avg) = take_interval();
    let line = row_line(r, max, avg, cpu_pct(), ram_mb());
    let mut file = file.lock().unwrap_or_else(|e| e.into_inner());
    let _ = file.write_all(line.as_bytes());
    let _ = file.flush();
}

/// One CSV line (with its newline) in [`HEADER`]'s column order.
fn row_line(r: &Row, max: f64, avg: f64, cpu_pct: f64, ram_mb: f64) -> String {
    let t_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    let n = r.frames.max(1.0);
    let card = |i: usize, scale: f64| r.card[i] as f64 / n / scale;
    let gpu_pct = if r.ms > 0.0 {
        r.gpu_ms / r.ms * 100.0
    } else {
        0.0
    };
    let columns = [
        r.frames,
        r.fps,
        r.ms,
        max,
        avg,
        r.cpu_prepare_ms,
        r.gpu_ms,
        gpu_pct,
        r.copies_ms,
        r.copies_mb,
        r.textures_ms,
        r.textures_mb,
        r.copyback_ms,
        r.copyback_mb,
        r.draws,
        r.dropped,
        r.making,
        r.indirect,
        r.heap_passes,
        r.heap_ms,
        r.texture_refresh_ms,
        r.card_passes,
        r.card_compares,
        card(0, 1e6),
        card(1, 1.0),
        card(2, 1.0),
        card(3, 1.0),
        card(4, 1e6),
        card(5, 1.0),
        card(6, 1e3),
        r.twins_into_depth,
        r.twins_into_colour,
        r.depth_planes,
        ram_mb,
        cpu_pct,
    ];
    let mut line = format!("{t_ms},{}", PRESENTED.load(Ordering::Relaxed));
    for value in columns {
        line.push_str(&format!(",{value:.3}"));
    }
    line.push('\n');
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_row_has_every_header_column() {
        let line = row_line(
            &Row {
                frames: 30.0,
                ms: 33.3,
                gpu_ms: 10.0,
                ..Row::default()
            },
            50.0,
            33.3,
            12.5,
            2048.0,
        );
        let header = HEADER.trim_end().split(',').count();
        assert_eq!(line.trim_end().split(',').count(), header);
        assert!(line.ends_with(",2048.000,12.500\n"));
    }

    #[test]
    fn switches_show_only_the_last_part_of_paths() {
        let vars = [
            ("VICTORY_FSR_DLL", r"D:\games\fsr\amd.dll"),
            ("VICTORY_QUEUE_COPIES", "1"),
            ("VICTORY_EMPTY", ""),
            ("PATH", r"C:\Windows"),
        ]
        .map(|(k, v)| (k.to_owned(), v.to_owned()));
        assert_eq!(
            switches_line(vars.into_iter()),
            "# switches: VICTORY_FSR_DLL=...\\amd.dll VICTORY_QUEUE_COPIES=1\n"
        );
        assert_eq!(switches_line(std::iter::empty()), "");
    }

    #[test]
    fn nvidia_public_driver_number() {
        assert_eq!(
            nvidia_driver_number("32.0.16.2124").as_deref(),
            Some("621.24")
        );
        assert_eq!(
            nvidia_driver_number("32.0.15.6094").as_deref(),
            Some("560.94")
        );
        assert_eq!(nvidia_driver_number("1.2"), None);
    }
}
