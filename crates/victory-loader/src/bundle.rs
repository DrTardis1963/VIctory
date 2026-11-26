//! The crash triage bundle: one self-contained file per crash.
//!
//! `crashes.log` keeps appending short reports; a bundle holds everything a
//! later reader (a person, `victory triage`, a test loop) needs about one
//! crash, without the process: the report with its registers and stack as
//! `module+rva`, the map of every mapped image (so an `rva` can be opened in
//! a disassembler), the last HLE calls, and the unimplemented imports the
//! title had called.
//!
//! The format is plain text: `key: value` header lines, a blank line, then
//! sections that each start with `== name ==`. [`Bundle::parse`] reads it
//! back on any host.

/// A bundle read back.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Bundle {
    /// The `key: value` lines before the first section, in order.
    pub header: Vec<(String, String)>,
    /// Each section's name and its lines, in order.
    pub sections: Vec<(String, Vec<String>)>,
}

/// One image of a bundle's module map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleModule {
    pub base: u64,
    pub size: u64,
    pub path: String,
}

impl BundleModule {
    /// The file name, as `module+rva` locations name the module.
    pub fn name(&self) -> &str {
        self.path.rsplit(['\\', '/']).next().unwrap_or(&self.path)
    }
}

/// The first line of every bundle; the number changes with the format.
pub const MAGIC: &str = "VIctory crash bundle 1";

impl Bundle {
    pub fn parse(text: &str) -> Bundle {
        let mut bundle = Bundle::default();
        let mut lines = text.lines();
        for line in lines.by_ref() {
            if line.trim().is_empty() {
                break;
            }
            if let Some((key, value)) = line.split_once(": ") {
                bundle.header.push((key.trim().into(), value.trim().into()));
            }
        }
        for line in lines {
            let name = line.strip_prefix("== ").and_then(|l| l.strip_suffix(" =="));
            match (name, bundle.sections.last_mut()) {
                (Some(name), _) => bundle.sections.push((name.into(), Vec::new())),
                (None, Some((_, body))) => body.push(line.into()),
                (None, None) => {}
            }
        }
        bundle
    }

    /// Whether `text` starts like a bundle.
    pub fn is_bundle(text: &str) -> bool {
        text.starts_with(MAGIC)
    }

    pub fn field(&self, key: &str) -> Option<&str> {
        self.header
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// The lines of section `name`; empty if there is none.
    pub fn section(&self, name: &str) -> &[String] {
        self.sections
            .iter()
            .find(|(n, _)| n == name)
            .map_or(&[], |(_, lines)| lines.as_slice())
    }

    /// The module map, from lines `base size path` (hex numbers).
    pub fn modules(&self) -> Vec<BundleModule> {
        let hex = |s: &str| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok();
        self.section("modules")
            .iter()
            .filter_map(|line| {
                let mut parts = line.trim().splitn(3, ' ');
                Some(BundleModule {
                    base: hex(parts.next()?)?,
                    size: hex(parts.next()?)?,
                    path: parts.next()?.trim().into(),
                })
            })
            .collect()
    }

    /// The image `location` (`name+0xrva`) is in, by its file name.
    pub fn module_named(&self, name: &str) -> Option<BundleModule> {
        self.modules()
            .into_iter()
            .find(|m| m.name().eq_ignore_ascii_case(name))
    }
}

/// `name+0xrva` split into its module name and rva.
pub fn split_location(location: &str) -> Option<(&str, u64)> {
    let (name, rva) = location.trim().rsplit_once('+')?;
    let rva = u64::from_str_radix(rva.strip_prefix("0x")?, 16).ok()?;
    Some((name, rva))
}

/// What a bundle's writer knows of the crash.
#[cfg(windows)]
pub(crate) struct Crash<'a> {
    /// The report as `crashes.log` gets it (without its end).
    pub report: &'a str,
    /// The faulting instruction (or the trapped call's return address).
    pub fault: Option<usize>,
}

/// Writes a bundle for `crash` into `dir` and returns its path. Called on
/// the crashing thread: what other threads may hold is only tried
/// (`try_lock`), and nothing waits.
#[cfg(windows)]
pub(crate) fn write(dir: &std::path::Path, crash: &Crash) -> Option<std::path::PathBuf> {
    use std::fmt::Write;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let pid = std::process::id();
    let mut text = String::with_capacity(16 << 10);
    let _ = writeln!(text, "{MAGIC}\nunix time: {now}\nprocess: {pid}");
    if let Some(at) = crash.fault {
        let _ = writeln!(text, "fault: {}\nfault address: {at:#x}", crate::locate(at));
        if let Some(start) = function_start(at) {
            let _ = writeln!(text, "fault function: {}", crate::locate(start));
        }
    }
    text.push_str("\n== report ==\n");
    text.push_str(crash.report);
    text.push_str("== modules ==\n");
    module_map(&mut text);
    text.push_str("== hle calls ==\n");
    text.push_str(&hle_calls());
    text.push_str("== unimplemented ==\n");
    text.push_str(&crate::thunks::report_lines());
    text.push_str("== title output ==\n");
    text.push_str(&crate::crash::title_output());

    for n in 0..100 {
        let name = match n {
            0 => format!("crash-{now}-{pid}.txt"),
            n => format!("crash-{now}-{pid}-{n}.txt"),
        };
        let path = dir.join(name);
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path);
        match file {
            Ok(mut file) => {
                use std::io::Write;
                return file.write_all(text.as_bytes()).is_ok().then_some(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return None,
        }
    }
    None
}

/// HLE calls kept for a bundle.
#[cfg(windows)]
const HLE_CALLS: usize = 64;

/// The last HLE calls, from the call trace's ring.
#[cfg(windows)]
fn hle_calls() -> String {
    crate::trace::recent_lines(HLE_CALLS)
}

/// The start of the function holding `pc`, by its image's unwind data.
#[cfg(windows)]
fn function_start(pc: usize) -> Option<usize> {
    use windows_sys::Win32::System::Diagnostics::Debug::RtlLookupFunctionEntry;
    let mut image = 0;
    // SAFETY: only looks up the function table covering `pc`.
    let entry = unsafe { RtlLookupFunctionEntry(pc as u64, &mut image, std::ptr::null_mut()) };
    // SAFETY: a non-null entry is the image's own, mapped `.pdata` record.
    (!entry.is_null()).then(|| image as usize + unsafe { (*entry).BeginAddress } as usize)
}

/// One line per mapped image: base, size, path. Read with the process-status
/// calls, which copy the loader's list without taking its lock.
#[cfg(windows)]
fn module_map(text: &mut String) {
    use std::fmt::Write;
    use windows_sys::Win32::Foundation::HMODULE;
    use windows_sys::Win32::System::ProcessStatus::{
        K32EnumProcessModules, K32GetModuleFileNameExW, K32GetModuleInformation, MODULEINFO,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // SAFETY: the pseudo-handle of this process; no preconditions.
    let process = unsafe { GetCurrentProcess() };
    let mut modules: Vec<HMODULE> = vec![std::ptr::null_mut(); 1024];
    let mut needed = 0;
    // SAFETY: the buffer and its byte length are passed together.
    let listed = unsafe {
        K32EnumProcessModules(
            process,
            modules.as_mut_ptr(),
            (modules.len() * size_of::<HMODULE>()) as u32,
            &mut needed,
        )
    };
    if listed == 0 {
        text.push_str("  (the module list could not be read)\n");
        return;
    }
    let count = (needed as usize / size_of::<HMODULE>()).min(modules.len());
    let mut path = vec![0u16; 1024];
    for &module in &modules[..count] {
        // SAFETY: a zeroed MODULEINFO is valid; the calls write the buffers
        // they are given, with their sizes.
        let (info, len) = unsafe {
            let mut info: MODULEINFO = std::mem::zeroed();
            K32GetModuleInformation(process, module, &mut info, size_of::<MODULEINFO>() as u32);
            let len =
                K32GetModuleFileNameExW(process, module, path.as_mut_ptr(), path.len() as u32);
            (info, len as usize)
        };
        let _ = writeln!(
            text,
            "  {:#x} {:#x} {}",
            module as usize,
            info.SizeOfImage,
            String::from_utf16_lossy(&path[..len.min(path.len())])
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "VIctory crash bundle 1\n\
        unix time: 100\n\
        process: 7\n\
        fault: title.exe+0x1234\n\
        fault address: 0x140001234\n\
        fault function: title.exe+0x1200\n\
        \n\
        == report ==\n\
        VIctory: unhandled exception 0xc0000005 at title.exe+0x1234 (read 0x10: nothing mapped there)\n\
        \x20 #0  title.exe+0x1234\n\
        == modules ==\n\
        \x20 0x140000000 0x20000 F:\\titles\\title.exe\n\
        \x20 0x7ffa00000000 0x1f0000 C:\\Windows\\SYSTEM32\\ntdll.dll\n\
        == unimplemented ==\n\
        \x20 unimplemented: a.dll!F x2 (first from title.exe+0x10)\n";

    #[test]
    fn a_bundle_reads_back_its_header_sections_and_modules() {
        assert!(Bundle::is_bundle(SAMPLE));
        let bundle = Bundle::parse(SAMPLE);
        assert_eq!(bundle.field("fault"), Some("title.exe+0x1234"));
        assert_eq!(bundle.field("process"), Some("7"));
        assert_eq!(bundle.section("report").len(), 2);
        assert!(bundle.section("hle calls").is_empty());
        let modules = bundle.modules();
        assert_eq!(modules.len(), 2);
        assert_eq!((modules[0].base, modules[0].size), (0x1_4000_0000, 0x20000));
        assert_eq!(modules[1].name(), "ntdll.dll");
        let title = bundle.module_named("TITLE.EXE").unwrap();
        assert_eq!(title.path, r"F:\titles\title.exe");
    }

    #[test]
    fn locations_split_into_module_and_rva() {
        assert_eq!(split_location("a b.exe+0x1f"), Some(("a b.exe", 0x1f)));
        assert_eq!(split_location("0x1234"), None);
        assert_eq!(split_location("x.dll+12"), None);
    }

    /// In a child process of the next test: faults on purpose, with crash
    /// reports going where `VICTORY_BUNDLE_DIR` says.
    #[cfg(windows)]
    #[test]
    fn bundle_child() {
        use windows_sys::Win32::System::Diagnostics::Debug::{
            EXCEPTION_POINTERS, SetUnhandledExceptionFilter,
        };
        let Some(dir) = std::env::var_os("VICTORY_BUNDLE_DIR") else {
            return;
        };
        /// As a title's own filter does: ours first, then it ends the
        /// process itself (no Windows error reporting to wait for).
        unsafe extern "system" fn title_filter(info: *const EXCEPTION_POINTERS) -> i32 {
            // SAFETY: the system passes the exception being dispatched.
            unsafe {
                crate::report_unhandled(info);
                windows_sys::Win32::System::Threading::ExitProcess(0xC0DE);
            }
        }
        crate::log_crashes_to(std::path::Path::new(&dir).join("crashes.log"));
        crate::report_crashes();
        // SAFETY: installs a filter that only reports and exits.
        unsafe { SetUnhandledExceptionFilter(Some(title_filter)) };
        // Two HLE calls for the bundle's trace section (the parent turns
        // the ring on); the second never returns.
        extern "system" fn answer(n: u64) -> u64 {
            n + 1
        }
        assert!(crate::trace::wanted(crate::Provenance::Hle, "Answer"));
        let stub = crate::trace::tests::traced("Answer", answer as *const () as usize);
        // SAFETY: the stub forwards to `answer`, which has this signature.
        let call: extern "system" fn(u64) -> u64 = unsafe { std::mem::transmute(stub) };
        assert_eq!(call(41), 42);
        extern "system" fn fault(_: u64) -> u64 {
            // SAFETY: none: it reads address 0x10, which is never mapped, so
            // the read faults before anything else happens.
            unsafe {
                std::arch::asm!("mov {x}, qword ptr [{p}]", p = in(reg) 0x10usize, x = out(reg) _);
            }
            0
        }
        let stub = crate::trace::tests::traced("Fault", fault as *const () as usize);
        // SAFETY: the stub forwards to `fault`, which has this signature.
        let call: extern "system" fn(u64) -> u64 = unsafe { std::mem::transmute(stub) };
        call(7);
        // SAFETY: none: it reads address 0x10, which is never mapped, so the
        // read faults before anything else happens.
        unsafe {
            std::arch::asm!("mov {x}, qword ptr [{p}]", p = in(reg) 0x10usize, x = out(reg) _);
        }
    }

    #[cfg(windows)]
    #[test]
    fn a_fault_writes_a_bundle_beside_the_crash_log() {
        let dir = tempfile::tempdir().unwrap();
        let exe = std::env::current_exe().unwrap();
        let out = std::process::Command::new(&exe)
            .args(["--exact", "bundle::tests::bundle_child", "--nocapture"])
            .env("VICTORY_BUNDLE_DIR", dir.path())
            .env(crate::trace::TRACE_ENV, "ring")
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(0xC0DE), "{out:?}");
        let found: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "txt"))
            .collect();
        assert_eq!(found.len(), 1, "{found:?}");
        let name = found[0].file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            name.starts_with("crash-") && name.ends_with(".txt"),
            "{name}"
        );
        let text = std::fs::read_to_string(&found[0]).unwrap();
        assert!(Bundle::is_bundle(&text), "{text}");
        let bundle = Bundle::parse(&text);

        // The fault is in this test's own executable, at a function start
        // its unwind data names.
        let exe_name = exe.file_name().unwrap().to_string_lossy().into_owned();
        let fault = bundle.field("fault").unwrap();
        let (module, rva) = split_location(fault).unwrap();
        assert!(module.eq_ignore_ascii_case(&exe_name), "{fault}");
        let (_, start) = split_location(bundle.field("fault function").unwrap()).unwrap();
        assert!(start <= rva, "{start:#x} > {rva:#x}");

        let report = bundle.section("report").join("\n");
        assert!(
            report.contains("unhandled exception 0xc0000005") && report.contains("read 0x10"),
            "{report}"
        );
        assert!(report.contains("  rax "), "{report}");
        assert!(report.contains("  #0 "), "{report}");
        // The module map names this executable by its path, and ntdll.
        let ours = bundle
            .module_named(&exe_name)
            .expect("the test exe is mapped");
        assert!(std::path::Path::new(&ours.path).is_file(), "{}", ours.path);
        assert!(rva < ours.size);
        assert!(bundle.module_named("ntdll.dll").is_some());
        for section in ["hle calls", "unimplemented", "title output"] {
            assert!(
                bundle.sections.iter().any(|(n, _)| n == section),
                "{section}"
            );
        }
        // The last HLE calls: one that returned, and the one that faulted.
        let calls = bundle.section("hle calls");
        let answer = calls.iter().find(|l| l.contains("test.dll!Answer"));
        let answer = answer.unwrap_or_else(|| panic!("{calls:?}"));
        assert!(
            answer.contains("args=[0x29,") && answer.contains("ret=0x2a"),
            "{answer}"
        );
        let last = calls.last().unwrap();
        assert!(
            last.contains("test.dll!Fault") && last.contains("not returned"),
            "{last}"
        );
        // The stack goes through the traced call, back to the test.
        assert!(bundle.section("report").len() > 4, "{report}");
        // The crash log names the bundle.
        let log = std::fs::read_to_string(dir.path().join("crashes.log")).unwrap();
        assert!(
            log.contains(&format!("  bundle: {}", found[0].display())),
            "{log}"
        );
    }
}
