//! Loader tests on images `victory-testkit` builds in memory: each has only
//! the parts its case is about. The same cases on the test binary itself,
//! the hle-probe fixture and a copy of a system DLL stay where they are.
#![cfg(windows)]

use victory_loader::{
    ImportResolver, Provenance, Resolution, UnimplementedPolicy, prepare, unimplemented_call_count,
};
use victory_pe::{ImageInfo, ImportSymbol};
use victory_testkit::{PeBuilder, Target, Unwind};

/// `mov eax, imm32; ret`.
fn returns(value: u32) -> Vec<u8> {
    let mut code = vec![0xB8];
    code.extend_from_slice(&value.to_le_bytes());
    code.push(0xC3);
    code
}

/// As `prepare`'s own test on the test binary: the copy is a DLL with no
/// entry point, no imports and no TLS callbacks, and its info still
/// describes the original.
#[test]
fn a_built_executable_is_prepared_for_mapping() {
    let mut pe = PeBuilder::exe("built.exe");
    pe.import("kernel32.dll", "GetTickCount");
    pe.import("xgameruntime.dll", "XGameRuntimeInitialize");
    let main = pe.code(&returns(0));
    let callback = pe.code(&[0xC3]);
    pe.entry(main).tls(&[0; 8], &[callback]);
    let original = pe.build().bytes;
    let prepared = prepare(&original).unwrap();
    assert!(!prepared.info.is_dll(), "info describes the original");
    assert_eq!(prepared.info.import_count(), 2);
    assert_eq!(prepared.info.entry_rva, main.rva());

    let patched = ImageInfo::parse(&prepared.bytes).unwrap();
    assert!(patched.is_dll());
    assert_eq!(patched.entry_rva, 0);
    assert!(patched.imports.is_empty());
    let tls = patched.tls.as_ref().unwrap();
    assert_eq!(tls.callbacks_va, 0);
    assert!(tls.callback_rvas.is_empty());
    assert_eq!(prepared.bytes.len(), original.len());
}

/// As `maps_a_title_dll_the_title_loads_itself`, with a built DLL in place
/// of a copy of `version.dll`: found by any spelling, mapped, its export
/// callable.
#[test]
fn a_built_title_dll_is_mapped_and_its_export_called() {
    let title_dir = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let mut pe = PeBuilder::dll("Late.dll");
    let answer = pe.code(&returns(42));
    pe.export("Answer", answer)
        .function(answer, 6, Unwind::leaf());
    std::fs::write(title_dir.path().join("Late.dll"), pe.build().bytes).unwrap();
    let load = |name: &str, wanted: &dyn Fn(&str) -> bool| {
        victory_loader::load_title_dll(name, title_dir.path(), cache.path(), wanted).unwrap()
    };
    let dlls = load("LATE.DLL", &|_| true);
    let names: Vec<_> = dlls.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["late.dll"]);
    let at = dlls[0].1.export("Answer").unwrap();
    // SAFETY: `Answer` is `mov eax, 42; ret`.
    let f: extern "system" fn() -> u32 = unsafe { std::mem::transmute(at) };
    assert_eq!(f(), 42);
    assert!(load("absent.dll", &|_| true).is_empty());
    assert!(load("late.dll", &|_| false).is_empty());
}

extern "system" fn fake_xgameruntime_initialize() -> i32 {
    0x1234
}

struct TestResolver;

impl ImportResolver for TestResolver {
    fn resolve(&self, dll: &str, symbol: &ImportSymbol) -> Resolution {
        match (
            dll.to_ascii_lowercase().as_str(),
            symbol.to_string().as_str(),
        ) {
            ("xgameruntime.dll", "XGameRuntimeInitialize") => Resolution::Found {
                address: fake_xgameruntime_initialize as *const () as usize,
                provenance: Provenance::Hle,
            },
            _ => Resolution::Missing,
        }
    }
}

/// As the hle-probe test's core: imports reach our implementation, an
/// unimplemented one reaches a stub that counts it, and TLS callbacks run.
#[test]
fn a_built_image_is_bound_and_calls_through_its_imports() {
    let mut pe = PeBuilder::exe("bound.exe");
    let init = pe.import("XGameRuntime.dll", "XGameRuntimeInitialize");
    let missing = pe.import("victory-probe-missing.dll", "NotImplemented");
    let counter = pe.data(&[0; 8]);
    let probe_init = pe.jump_to(init);
    let probe_missing = pe.jump_to(missing);
    // inc dword ptr [rip+disp32]; ret
    let callback = pe.code_with(
        &[0xFF, 0x05, 0, 0, 0, 0, 0xC3],
        &[(2, Target::Data(counter))],
    );
    let main = pe.code(&returns(0));
    pe.entry(main)
        .export("probe_init", probe_init)
        .export("probe_missing", probe_missing)
        .tls(&[0; 8], &[callback]);
    let built = pe.build();
    let prepared = prepare(&built.bytes).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let image = victory_loader::load(&prepared, dir.path()).unwrap();
    let report = image
        .bind_imports(&TestResolver, UnimplementedPolicy::ReturnZero)
        .unwrap();
    assert_eq!((report.hle, report.host), (1, 0));
    assert_eq!(report.missing.len(), 1);
    assert_eq!(report.missing[0].symbol, "NotImplemented");

    let call = |name: &str| image.export(name).unwrap_or_else(|| panic!("{name}"));
    // SAFETY: the exports jump through bound import slots to functions
    // without arguments; the callback only increments the counter.
    unsafe {
        let f: extern "system" fn() -> i32 = std::mem::transmute(call("probe_init"));
        assert_eq!(f(), 0x1234, "console API must reach our implementation");
        let before = unimplemented_call_count();
        let f: extern "system" fn() -> u64 = std::mem::transmute(call("probe_missing"));
        assert_eq!(f(), 0);
        assert_eq!(unimplemented_call_count(), before + 1);
        let counter = (image.base() + built.data_rva(counter) as usize) as *const u32;
        assert_eq!(counter.read_volatile(), 0);
        image.run_tls_callbacks(victory_loader::DLL_PROCESS_ATTACH);
        assert_eq!(counter.read_volatile(), 1);
        image.unload();
    }
}

/// In a child process of the next test: binds a built image with the HLE
/// call trace on (`VICTORY_HLE_TRACE`, read once per process) and calls
/// through its import twice.
#[test]
fn traced_child() {
    if std::env::var_os("VICTORY_BUILT_TRACE_CHILD").is_none() {
        return;
    }
    let mut pe = PeBuilder::dll("traced.dll");
    let init = pe.import("xgameruntime.dll", "XGameRuntimeInitialize");
    let probe = pe.jump_to(init);
    pe.export("probe", probe);
    let prepared = prepare(&pe.build().bytes).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let image = victory_loader::load(&prepared, dir.path()).unwrap();
    image
        .bind_imports(&TestResolver, UnimplementedPolicy::ReturnZero)
        .unwrap();
    // SAFETY: `probe` jumps through the bound slot to a function without
    // arguments.
    let f: extern "system" fn() -> i32 =
        unsafe { std::mem::transmute(image.export("probe").unwrap()) };
    assert_eq!(f(), 0x1234);
    assert_eq!(f(), 0x1234);
    // The trace's writer thread writes every 50 ms.
    std::thread::sleep(std::time::Duration::from_millis(500));
}

#[test]
fn a_bound_hle_import_is_traced_when_asked() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("trace.ndjson");
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "traced_child", "--nocapture"])
        .env("VICTORY_BUILT_TRACE_CHILD", "1")
        .env("VICTORY_HLE_TRACE", &file)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let text = std::fs::read_to_string(&file).unwrap();
    let calls: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("\"fn\":\"XGameRuntimeInitialize\""))
        .collect();
    assert_eq!(calls.len(), 2, "{text}");
    // The image's thunk jumps, so the caller is the code that called it:
    // this test's executable.
    let exe = std::env::current_exe().unwrap();
    let exe = exe.file_name().unwrap().to_string_lossy().into_owned();
    assert!(calls[0].contains("\"dll\":\"xgameruntime.dll\""), "{text}");
    assert!(
        calls[0].contains(&format!("\"caller\":\"{exe}+0x")),
        "{text}"
    );
    assert_eq!(text.matches("\"ret\":4660}").count(), 2, "{text}");
}
