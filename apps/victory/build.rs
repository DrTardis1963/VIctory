// (Local test patch, not the fork's) Exports the D3D12 Agility SDK symbols
// from the executable, so Windows 10 loads the runtime in .\D3D12\.
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
    {
        println!("cargo:rustc-link-arg-bins=/INCLUDE:D3D12SDKVersion");
        println!("cargo:rustc-link-arg-bins=/INCLUDE:D3D12SDKPath");
        println!("cargo:rustc-link-arg-bins=/EXPORT:D3D12SDKVersion,DATA");
        println!("cargo:rustc-link-arg-bins=/EXPORT:D3D12SDKPath,DATA");
    }
}
