# Preserve GPU execution ordering for buffer copies

## Reproduction

`CopyBufferRegion` and buffer-to-buffer `CopyResource` queue CPU copies through
`HostList::defer_copy`. `Host::execute` runs those steps and flushes the shadow
refresh batch before submitting the native lists. A GPU store earlier in the
title list therefore happens after the CPU copy. A GPU read earlier in the list
can also see the shadow refresh caused by a later copy.

The tests allocate title resources and lists through VIctory. They call the real
title copy entry points and record original compute shaders against the same
native shadows that VIctory refreshes and copies back. The original list writes
`500` into A, copies A to B, and loads B into a third buffer. The baseline on
AMD ended with A equal to `500`, B equal to `13`, and the shader output equal to
`13`. In the inverse case, a shader before the copy observed `27` instead of
the original `13`. The chain, cross-list batch, and `CopyResource` cases also
expose stale data.

Run the known failures on Windows with a D3D12 adapter. The bounds, prior
submission, resubmission, staging fill, and explicit native-state controls run
with the same opt-in and without `--ignored`. The suite reports the actual
adapter and fails if requested validation is inactive.

```powershell
$env:VICTORY_TEST_BUFFER_COPY = '1'
cargo test -p victory-gpu --features diagnostics buffer_copy_tests -- --ignored --nocapture --test-threads=1
```

Set `VICTORY_D3D12_DEBUG=1` to check debug-layer errors. Set
`VICTORY_TEST_BUFFER_COPY_GBV=1` for GPU-based validation with resource-state
tracking. Run the filtered suite in its own process so it can configure
validation before the first device is created. `VICTORY_D3D12_WARP=1` selects
the software adapter in diagnostics builds.

## GPU ordering and state

Use native `CopyBufferRegion` with the existing shadow refresh and copyback
machinery when the transfer can stay on the GPU timeline. Retain CPU copies
when dependencies prove their ordering is equivalent. Use the logical title
size for a buffer-to-buffer `CopyResource` and require matching title sizes.
The shadows have rounded widths
and remain mapped. Native `CopyResource` requires matching sizes, distinct
resources, and unmapped subresources. [Microsoft CopyResource restrictions](https://learn.microsoft.com/en-us/windows/win32/api/d3d12/nf-d3d12-id3d12graphicscommandlist-copyresource#restrictions)

Select the buffer barrier model before the first native access within each
`ExecuteCommandLists` call. For enhanced barriers, a prelude can register the
participating buffers while they are in `COMMON`. At each copy, order preceding
writes and reads against source and destination access, then publish the copy
writes for following reads and writes. Include the same resource when it is
used as an indirect argument. Main's indirect clamp currently leaves its
argument shadow in legacy UAV state. A buffer already promoted or transitioned
to a legacy state needs a handoff to `COMMON` before it can enter enhanced
tracking. Use a shared access contract for copy and indirect code so neither
subsystem has to infer the other's native state. [Microsoft barrier interoperability](https://microsoft.github.io/DirectX-Specs/d3d/D3D12EnhancedBarriers.html#interop-with-legacy-resourcebarrier) · [PR #40](https://github.com/Marvisak/VIctory/pull/40)

On legacy adapters, an explicit transition works when the preceding native
state is known. A title buffer can have a promoted read combination or UAV
access before the copy. Some directly indexed accesses choose their resource at
GPU runtime. A legacy transition with an assumed `BeforeState` cannot cover
these cases. An intermediate buffer still requires legal states on both
original endpoints. Buffers decay between native submissions. Lists submitted
together share the same decay boundary. [Microsoft promotion and decay rules](https://learn.microsoft.com/en-us/windows/win32/direct3d12/using-resource-barriers-to-synchronize-resource-states-in-direct3d-12#implicit-state-transitions)

For a general legacy path, isolate runs of copies in separate native submission
groups and use the buffer decay at group boundaries. Place the GPU-only groups
in queue order and wait for completion once at the end. This needs a way to
restore native render state on continuation lists, including descriptor heaps,
pipeline and root bindings, primitive topology, and dynamic state. Verify
query and predication behavior across the same boundaries. Group consecutive
copies only when their dependencies and transitions fit within a group.
[Microsoft queue ordering rules](https://learn.microsoft.com/en-us/windows/win32/api/d3d12/nf-d3d12-id3d12commandqueue-executecommandlists#remarks)

## CPU staging and shadow coherency

The shadow batch should refresh the source range and the destination's initial
bytes, then register the destination as pending before any later refresh can
overwrite it. Source refresh must respect an earlier pending GPU write. The
destination's initial snapshot matters when copying zeros into a dirty title
buffer. An uninitialized zero shadow would make that write invisible to the
existing differential copyback. After the fence, merge overlapping destination
ranges by shadow and reconcile them once, using the existing change detection
to preserve unrelated title writes. Retire all covered pending ranges and
perform CPU completion writes after the copyback.

Preparation also consumes raw title memory. Buffer-to-texture upload staging,
unaligned root CBV scratch, and CPU rectangle expansion read bytes before native
submission. A deferred copy currently feeds those consumers when its input is
already available on the CPU. Moving the copy to the GPU would leave a consumer
of the destination reading old bytes. Track those source ranges and their
position in the recorded stream. Keep a fill in preparation when its input is
provably CPU-authoritative at that point. A GPU-produced upload can use an
ordered GPU transfer, or a targeted dependency boundary where a CPU
transformation genuinely requires the bytes. This retains the distinction
between copies into title memory and fills of host upload mappings.
[Wine staging report #128](https://github.com/Marvisak/VIctory/issues/128#issuecomment-7118712118)

A temporary native-copy prototype moved `27` into the title destination but
left a following CPU staging fill at `13`. The baseline filled `27` in the
same sequence. The staging test exercises the actual deferred fill with a
native upload mapping and records the behavior the fix must preserve.

## Bounds, aliases, and lifetime

- Check both endpoint lengths and all offset and address arithmetic. Accept
  exact-end and zero-byte calls with the usual bounds checks.
- Identify aliases by native resource identity and shadow offsets, including
  different title objects resolving to the same shadow. Identical ranges can
  be skipped. Reject intersecting ranges. Disjoint same-resource copies need
  enhanced barriers or a two-stage legacy transfer with scratch storage.
  Distinct shadows over overlapping title memory need their own coherence rule.
  [Microsoft self-copy rules](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/enhanced-barriers#self-resource-copy)
- Retain the native shadows and the title backing needed for refresh and
  copyback until the recorded list releases its references. Validate the live
  mapping or generation for externally managed backing before accessing it.
  A stale title interface supplied to a new ABI call is already invalid input.

## Validation and prototype costs

Microsoft's Agility SDK 1.620.2 provided an active debug layer on this
workstation after Graphics Tools installed. The inbox runtime returned
`E_INVALIDARG` when creating a debug device. The app-local test executable
verified the debug message queue and GPU-based validation with state tracking
enabled. The GPU test reported the selected adapter on every case.

AMD Radeon Graphics, NVIDIA RTX 5070 Laptop GPU, and Microsoft WARP reproduced
the five known ordering failures and passed five controls. A temporary test
build selected the NVIDIA adapter explicitly. The native reference with
explicit states read `500` and copied `500` back to B on all three. The reference is a control
for the expected behavior. The other tests call the title copy entry points.

The temporary shim reused shadow refresh and copyback and called native
`CopyBufferRegion`. It produced `500` on AMD without validation. WARP with
GPU-based validation reported error 538 for using a UAV as a copy source and
then a copy destination as a shader resource. Adding an enhanced barrier at
the copy reported error 1351 because the UAV was still in a legacy state. A
legal native reference and a correct runtime implementation both need the
producer and consumer states accounted for.

The CPU-only microbenchmark reports medians from three runs of 32 warm
submissions on AMD with validation disabled. Timing includes VIctory's normal
end-of-submission fence wait. The plain native prototype had
two cached native shadows per case and added no queue waits for these cases.
GPU execution timestamps were outside this probe. The benchmark measures the
copy-only prototype, which cannot be used for the mixed-state cases above.

| Work per submission | Preparation µs, baseline / prototype | Fence wait µs, baseline / prototype | Wall µs, baseline / prototype | Prototype refresh / copyback bytes |
| --- | ---: | ---: | ---: | ---: |
| One 4-byte copy | 107 / 394 | 134 / 5,882 | 324 / 6,861 | 256 / 128 |
| One 1 MiB copy | 1,921 / 9,011 | 197 / 21,684 | 2,188 / 59,042 | 67,108,864 / 2,097,152 |
| 128 four-byte copies | 9,775 / 19,307 | 553 / 16,912 | 10,522 / 49,624 | 32,768 / 128 |

The baseline moved CPU-only copy bytes directly in preparation and reported
zero shadow refresh and copyback bytes. The prototype refreshed both endpoints
and reconciled the destination. The additional transfers give a reason to
keep a CPU copy when the command stream proves it is equivalent.

## Implementation checkpoint

The stale copy and early-read failures are reproducible. A general native
replacement needs decisions about buffer access with indirect execution,
legacy continuation lists, and ordered consumers of CPU staging. This branch
contains the regression fixture and the design checkpoint. The title copy path
still uses the baseline implementation.

Queue synchronization, texture readback, and faster shadow copyback each
address a different part of the pipeline. Directly indexed SRV refresh can
make a shadow current at submission, while the copy still has to occur between
the producer and consumer on the GPU timeline. [PR #32](https://github.com/Marvisak/VIctory/pull/32) · [PR #67](https://github.com/Marvisak/VIctory/pull/67) · [PR #124](https://github.com/Marvisak/VIctory/pull/124) · [PR #185](https://github.com/Marvisak/VIctory/pull/185)

Before proposing the runtime change, require the focused failures to pass with
the debug layer and resource-state tracking active, including copy operations
before and after indirect execution, aliased or released resources, and a
dependent host upload. Measure CPU preparation, GPU execution, bytes refreshed
and copied back, allocation count, synchronization, and wall time for small and
large copies, repeated copies, CPU and GPU producers, and dependent lists.
Measure enhanced and legacy paths individually because native submission
boundaries affect the legacy cost.
