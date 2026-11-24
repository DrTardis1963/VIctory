//! A title DLL that cannot be loaded is named in the error.
#![cfg(windows)]

#[test]
fn a_title_dll_that_does_not_prepare_is_named() {
    let title = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    std::fs::write(title.path().join("broken.dll"), b"MZ not a PE").unwrap();
    let wanted = |_: &str| true;
    let loaded = victory_loader::load_title_dll("broken.dll", title.path(), cache.path(), &wanted);
    let Err(error) = loaded else {
        panic!("a DLL that is not a PE loaded");
    };
    let text = format!("{error:#}");
    assert!(text.contains("broken.dll"), "{text}");
}

#[test]
fn traversal_and_absolute_dll_names_do_not_escape_title_dir() {
    let root = tempfile::tempdir().unwrap();
    let title = root.path().join("title");
    let sub = title.join("sub");
    let cache = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(root.path().join("escape.dll"), b"MZ not a PE").unwrap();
    std::fs::write(sub.join("nested.dll"), b"MZ not a PE").unwrap();

    let abs = root.path().join("escape.dll");
    let cases = [
        "..\\escape.dll",
        "../escape.dll",
        abs.to_str().unwrap(),
        "sub\\nested.dll",
        "sub/nested.dll",
        "..",
        "C:escape.dll",
        "escape.dll:stream",
    ];
    for name in cases {
        let loaded = victory_loader::load_title_dll(name, &title, cache.path(), &|_| true).unwrap();
        assert!(loaded.is_empty(), "{name} should be rejected");
    }
}
