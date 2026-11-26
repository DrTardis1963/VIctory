//! Xbox Series shaders to host shaders.
//!
//! An Xbox Series shader is a standard DXBC container ([`dxbc`]). Its `DXIL`
//! part ([`dxil`]) may carry Xbox additions around the LLVM bitcode: an
//! `XDAT` table before it indexing blobs after it, among them `xbop`
//! (compiler options) and the zlib-compressed GFX10 code. The bitcode may use
//! console-only intrinsics, so the GFX10 code is translated in general.

pub mod bvh;
pub mod dxbc;
pub mod dxil;
pub mod emit;
pub mod gfx10;
pub mod io;
pub mod ir;
pub mod native;
pub mod resource;
pub mod spill;
pub mod structure;

/// Opts into the D3D12 Agility SDK beside the executable (`.\D3D12\`), as our
/// spine does (c8858843): Windows 10's own runtime gives no SM 6.6 device.
#[cfg(all(target_os = "windows", target_env = "msvc"))]
pub mod d3d12_agility {
    #[repr(transparent)]
    pub struct SyncPtr(*const u8);

    // SAFETY: `D3D12SDKPath` points to a `'static` NUL-terminated ASCII string.
    unsafe impl Sync for SyncPtr {}

    #[used]
    #[allow(non_upper_case_globals)]
    #[unsafe(no_mangle)]
    pub static D3D12SDKVersion: u32 = 614;

    #[used]
    #[allow(non_upper_case_globals)]
    #[unsafe(no_mangle)]
    pub static D3D12SDKPath: SyncPtr = SyncPtr(c".\\D3D12\\".as_ptr().cast());
}

pub use dxbc::{Container, FourCC, Part};
pub use dxil::{DxilProgram, ShaderKind, XboxExtras};
pub use native::{
    HwStage, NativeProgram, ResourceType, RootParam, RootType, UsedResource, native_programs,
};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ShaderError {
    #[error("not a DXBC container")]
    NotDxbc,
    #[error("malformed shader: {0}")]
    Malformed(&'static str),
    #[error("GFX10 code at {offset:#x}: {reason}")]
    Isa { offset: usize, reason: &'static str },
    /// Valid code the translator cannot handle yet: an opcode name or a construct.
    #[error("GFX10 code at {offset:#x}: {what} is not supported")]
    Unsupported { offset: usize, what: &'static str },
}
