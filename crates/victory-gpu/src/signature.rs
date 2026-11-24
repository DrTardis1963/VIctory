//! Command signatures: the record layout of indirect execution. Kept for
//! translating it later.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use crate::object::{
    self, COMMAND_SIGNATURE, E_INVALIDARG, HRESULT, Iid, Object, Table, This, guid,
};
use crate::resource::PAGEABLE_IIDS;

const SIGNATURE_IIDS: [Iid; 4] = [
    guid(
        0xc36a797c,
        0xec80,
        0x4f0a,
        [0x89, 0x85, 0xa7, 0xb2, 0x47, 0x50, 0x82, 0xd1],
    ),
    PAGEABLE_IIDS[0],
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];

#[repr(C)]
pub(crate) struct Desc {
    stride: u32,
    count: u32,
    /// Each a type and up to three 32-bit fields.
    arguments: *const [u32; 4],
    nodes: u32,
}

#[repr(C)]
struct Signature {
    object: Object,
    stride: u32,
    arguments: Vec<[u32; 4]>,
}

/// Device method.
pub(crate) extern "system" fn create(
    _: This,
    desc: *const Desc,
    _root: This,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes a description (null is tolerated).
    let Some(desc) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    if desc.arguments.is_null() && desc.count > 0 {
        return E_INVALIDARG;
    }
    let arguments = if desc.count == 0 {
        Vec::new()
    } else {
        // SAFETY: the title passes `count` argument descriptions.
        unsafe { std::slice::from_raw_parts(desc.arguments, desc.count as usize) }.to_vec()
    };
    tracing::debug!(stride = desc.stride, ?arguments, "command signature");
    let signature = Box::new(Signature {
        object: Object::new(&FUNCTIONS),
        stride: desc.stride,
        arguments,
    });
    object::hand_out(COMMAND_SIGNATURE, signature, iid, &SIGNATURE_IIDS, out)
}

/// A signature's record stride and arguments (type, then up to three
/// fields each).
pub(crate) fn layout<'a>(this: This) -> Option<(u32, &'a [[u32; 4]])> {
    if this.is_null() {
        return None;
    }
    // SAFETY: the title passes one of our signatures.
    let s = unsafe { &*this.cast::<Signature>() };
    Some((s.stride, &s.arguments))
}

extern "system" fn query(this: This, iid: *const Iid, out: *mut This) -> HRESULT {
    let found = object::is(iid, &SIGNATURE_IIDS).then_some(this);
    object::answer(COMMAND_SIGNATURE, iid, found, out)
}
extern "system" fn release(this: This) -> u32 {
    object::release_boxed::<Signature>(this)
}

/// IUnknown 3, object 4, device child 1.
static FUNCTIONS: Table<24> = {
    let mut f = object::d3d12_object(object::missing_table::<COMMAND_SIGNATURE, 24>());
    f[0] = query as *const ();
    f[2] = release as *const ();
    f[7] = object::parent_device as *const (); // 0x38
    Table(f)
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{E_NOINTERFACE, S_FALSE, S_OK, slot};

    type QueryInterface = extern "system" fn(This, *const Iid, *mut This) -> HRESULT;
    type AddRef = extern "system" fn(This) -> u32;
    type Release = extern "system" fn(This) -> u32;

    #[test]
    fn command_signature_validates_desc_exposes_layout_and_releases() {
        let null = std::ptr::null();
        let mut out = null;
        assert_eq!(
            create(null, std::ptr::null(), null, &SIGNATURE_IIDS[0], &mut out),
            E_INVALIDARG
        );

        let null_args_nonzero = Desc {
            stride: 16,
            count: 1,
            arguments: std::ptr::null(),
            nodes: 0,
        };
        assert_eq!(
            create(null, &null_args_nonzero, null, &SIGNATURE_IIDS[0], &mut out),
            E_INVALIDARG
        );

        let empty_desc = Desc {
            stride: 16,
            count: 0,
            arguments: std::ptr::null(),
            nodes: 0,
        };
        assert_eq!(
            create(null, &empty_desc, null, &SIGNATURE_IIDS[0], &mut out),
            S_OK
        );
        assert_eq!(layout(out), Some((16, &[][..])));
        assert_eq!(slot::<Release>(out, 0x10)(out), 0);

        let args = [[0u32, 0, 0, 0], [5, 1, 2, 3]];
        let desc = Desc {
            stride: 24,
            count: args.len() as u32,
            arguments: args.as_ptr(),
            nodes: 0,
        };
        assert_eq!(
            create(null, &desc, null, &SIGNATURE_IIDS[0], std::ptr::null_mut()),
            S_FALSE
        );

        let mut sig = null;
        assert_eq!(
            create(null, &desc, null, &SIGNATURE_IIDS[0], &mut sig),
            S_OK
        );
        assert_eq!(layout(sig), Some((24, &args[..])));
        assert_eq!(layout(null), None);

        let qi = slot::<QueryInterface>(sig, 0x00);
        let add = slot::<AddRef>(sig, 0x08);
        let rel = slot::<Release>(sig, 0x10);
        for iid in &SIGNATURE_IIDS {
            let mut iface = null;
            assert_eq!(qi(sig, iid, &mut iface), S_OK);
            assert_eq!(iface, sig);
            assert_eq!(add(sig), 3);
            assert_eq!(rel(sig), 2);
            assert_eq!(rel(sig), 1);
        }

        let mut dev = null;
        assert_eq!(
            slot::<QueryInterface>(sig, 0x38)(sig, &crate::device::IID, &mut dev),
            S_OK
        );
        assert_eq!(dev, crate::device::device());

        let mut bad = sig;
        assert_eq!(qi(sig, &[0; 16], &mut bad), E_NOINTERFACE);
        assert!(bad.is_null());

        assert_eq!(rel(sig), 0);
    }
}
