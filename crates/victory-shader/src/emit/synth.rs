//! Synthetic programs through the whole translator, for the snapshot and
//! fuzz tests: the compute and pixel programs they build, the pipeline from
//! code to DXIL, and the DXIL validator.

use super::HostShader;
use crate::ShaderError;
use crate::dxbc::{Container, FourCC};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{io, ir, resource, structure};

/// VGPRs the synthetic compute programs declare.
pub(super) const VGPRS: u16 = 16;

pub(super) fn bytes(words: &[u32]) -> Vec<u8> {
    words.iter().flat_map(|w| w.to_le_bytes()).collect()
}

/// A one-group compute program reading four root constants in `s0`-`s3` and
/// a descriptor table address in `s4` whose first entry is a raw UAV, as the
/// VALU tests' programs do. `threads` lanes run (thread index in `v0`);
/// `lds_bytes` is a multiple of 512.
pub(super) fn compute(code: Vec<u8>, threads: u32, lds_bytes: u32) -> NativeProgram {
    NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: VGPRS,
        layout: vec![
            RootParam {
                root: RootType::Constants,
                table_offset_dwords: 0,
                sgprs: 4,
                base_register: 0,
                user_data: 0,
                slot: 0,
            },
            RootParam {
                root: RootType::Table,
                table_offset_dwords: 0,
                sgprs: 1,
                base_register: 0,
                user_data: 4,
                slot: 1,
            },
        ],
        used: vec![UsedResource {
            kind: ResourceType::Uav,
            root: RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 0,
            dwords: 4,
            user_data: 4,
            slot: 1,
        }],
        io: StageIo::Cs(ComputeIo {
            rsrc2: (5 << 1) | ((lds_bytes / 512) << 15),
            threads: [threads, 1, 1],
        }),
        code,
    }
}

/// A pixel program with no inputs that exports one float4 render target.
#[cfg(windows)]
pub(super) fn pixel(code: Vec<u8>) -> NativeProgram {
    NativeProgram {
        stage: HwStage::Ps,
        wave64: false,
        vgprs: VGPRS,
        layout: vec![],
        used: vec![],
        io: StageIo::Ps {
            inputs: vec![],
            input_addr: 0,
            db_shader_control: 0,
            patches: vec![],
        },
        code,
    }
}

/// The container parts a program needs beside its code: a pixel program's
/// output signature (one float4 render target); nothing for compute.
fn signatures(native: &NativeProgram) -> Vec<u8> {
    if native.stage != HwStage::Ps {
        return crate::dxbc::build(&[]);
    }
    let target = super::container::SigElement {
        name: "SV_Target".into(),
        index: 0,
        kind: 16,
        comp: 9,
        interp: 0,
        row: Some(0),
        col: 0,
        cols: 4,
        usage: 15,
        half: false,
    };
    let osg1 = super::container::signature(&[target], false);
    crate::dxbc::build(&[(FourCC::OSG1, &osg1)])
}

/// IR, structurizing, resources, IO and lowering: what `emit::translate`
/// runs after unpacking a title's container.
pub(super) fn translate(native: &NativeProgram) -> Result<HostShader, ShaderError> {
    let parts = signatures(native);
    let c = Container::parse(&parts)?;
    let p = ir::build(&native.patched_code(), native.wave64, native.vgprs)?;
    let st = structure::structure(&p)?;
    let res = resource::track(&p, &native.layout, native.stage, Some(&st))?;
    let io = io::raise(&p, native)?;
    super::emit(&c, native, &p, &st, &io, &res)
}

/// `dxil.dll`, when installed.
#[cfg(windows)]
pub(super) fn validator() -> Option<super::validator::Validator> {
    super::validator::Validator::load(super::validator::Validator::default_path().as_deref()).ok()
}

/// Checks `dxil` with `v`; the validator's text if it is rejected.
#[cfg(windows)]
pub(super) fn validate(v: &mut super::validator::Validator, dxil: &[u8]) -> Result<(), String> {
    v.validate(&mut dxil.to_vec())
}
