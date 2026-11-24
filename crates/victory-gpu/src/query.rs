//! Query heaps. Nothing runs on a GPU yet, so no query ever produces a
//! result; the heap only remembers its description.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use crate::object::{self, E_INVALIDARG, HRESULT, Iid, Object, QUERY_HEAP, Table, This, guid};
use crate::resource::PAGEABLE_IIDS;

const HEAP_IIDS: [Iid; 4] = [
    guid(
        0x0d9658ae,
        0xed45,
        0x469e,
        [0xa6, 0x1d, 0x97, 0x0e, 0xc5, 0x83, 0xca, 0xb4],
    ),
    PAGEABLE_IIDS[0],
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct Desc {
    kind: u32,
    count: u32,
    nodes: u32,
}

#[repr(C)]
struct Heap {
    object: Object,
    desc: Desc,
}

/// Device method.
pub(crate) extern "system" fn create(
    _: This,
    desc: *const Desc,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes a description (null is tolerated).
    let Some(&desc) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    tracing::debug!(?desc, "query heap");
    let heap = Box::new(Heap {
        object: Object::new(&FUNCTIONS),
        desc,
    });
    object::hand_out(QUERY_HEAP, heap, iid, &HEAP_IIDS, out)
}

extern "system" fn query(this: This, iid: *const Iid, out: *mut This) -> HRESULT {
    object::answer(
        QUERY_HEAP,
        iid,
        object::is(iid, &HEAP_IIDS).then_some(this),
        out,
    )
}
extern "system" fn release(this: This) -> u32 {
    object::release_boxed::<Heap>(this)
}

/// IUnknown 3, object 4, device child 1.
static FUNCTIONS: Table<24> = {
    let mut f = object::d3d12_object(object::missing_table::<QUERY_HEAP, 24>());
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
    fn query_heap_validates_desc_and_supports_com_lifecycle() {
        let null = std::ptr::null();
        let mut out = null;
        assert_eq!(
            create(null, std::ptr::null(), &HEAP_IIDS[0], &mut out),
            E_INVALIDARG
        );

        let desc = Desc {
            kind: 2,
            count: 64,
            nodes: 0,
        };
        let mut rejected = std::ptr::without_provenance::<Object>(1);
        assert_eq!(create(null, &desc, &[0; 16], &mut rejected), E_NOINTERFACE);
        assert!(rejected.is_null());

        assert_eq!(
            create(null, &desc, &HEAP_IIDS[0], std::ptr::null_mut()),
            S_FALSE
        );

        let mut heap = null;
        assert_eq!(create(null, &desc, &HEAP_IIDS[0], &mut heap), S_OK);
        let qi = slot::<QueryInterface>(heap, 0x00);
        let add = slot::<AddRef>(heap, 0x08);
        let rel = slot::<Release>(heap, 0x10);
        for iid in &HEAP_IIDS {
            let mut iface = null;
            assert_eq!(qi(heap, iid, &mut iface), S_OK);
            assert_eq!(iface, heap);
            assert_eq!(add(heap), 3);
            assert_eq!(rel(heap), 2);
            assert_eq!(rel(heap), 1);
        }

        let mut dev = null;
        assert_eq!(
            slot::<QueryInterface>(heap, 0x38)(heap, &crate::device::IID, &mut dev),
            S_OK
        );
        assert_eq!(dev, crate::device::device());

        assert_eq!(rel(heap), 0);
    }
}
