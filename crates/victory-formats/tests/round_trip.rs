//! Property tests of the texture layouts: for random descriptions in every
//! GFX10 swizzle mode, tiling rows and detiling them gives the rows back,
//! no two elements share an address, and detiling in bands equals detiling
//! whole. The fixed cases in `texture.rs` pin known layouts; these look for
//! the sizes nobody thought of.
//!
//! `VICTORY_TILE_CASES=<n>` runs more cases (default 300) and
//! `VICTORY_TILE_SEED=<n>` another sequence; a failure names its seed and
//! description.

use victory_formats::texture::{Desc, Dimension, Error, Layout, Swizzle};

/// xorshift64*: small, seeded, and the same on every machine.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        items[self.below(items.len() as u64) as usize]
    }
}

const MODES: [Swizzle; 16] = [
    Swizzle::LINEAR,
    Swizzle::SW_256B_S,
    Swizzle::SW_256B_D,
    Swizzle::SW_4KB_S,
    Swizzle::SW_4KB_D,
    Swizzle::SW_64KB_S,
    Swizzle::SW_64KB_D,
    Swizzle::SW_64KB_S_T,
    Swizzle::SW_64KB_D_T,
    Swizzle::SW_4KB_S_X,
    Swizzle::SW_4KB_D_X,
    Swizzle::SW_4KB_R_X,
    Swizzle::SW_64KB_Z_X,
    Swizzle::SW_64KB_S_X,
    Swizzle::SW_64KB_D_X,
    Swizzle::SW_64KB_R_X,
];

fn env(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// A random description, small enough that a case takes milliseconds.
fn random_desc(rng: &mut Rng) -> Desc {
    let dimension = rng.pick(&[
        Dimension::Tex2d,
        Dimension::Tex2d,
        Dimension::Tex3d,
        Dimension::Tex1d,
    ]);
    let block = if dimension == Dimension::Tex2d && rng.below(4) == 0 {
        4
    } else {
        1
    };
    let element_bytes = if block == 4 {
        rng.pick(&[8, 16])
    } else {
        rng.pick(&[1, 2, 4, 8, 16])
    };
    // Mostly small and odd sizes, sometimes a power of two or a large one.
    let size = |rng: &mut Rng| match rng.below(6) {
        0 => 1 << rng.below(9),
        1 => 1 + rng.below(600) as u32,
        _ => 1 + rng.below(140) as u32,
    };
    let width = size(rng);
    let height = if dimension == Dimension::Tex1d {
        1
    } else {
        size(rng)
    };
    let depth = match dimension {
        Dimension::Tex3d => 1 + rng.below(20) as u32,
        _ => 1 + rng.below(4) as u32,
    };
    let largest = width.max(height).max(if dimension == Dimension::Tex3d {
        depth
    } else {
        1
    });
    let full_chain = 32 - largest.leading_zeros();
    let mips = 1 + rng.below(u64::from(full_chain)) as u32;
    Desc {
        dimension,
        width,
        height,
        depth,
        mips,
        element_bytes,
        block,
        samples: 1,
        swizzle: rng.pick(&MODES),
    }
}

/// Tiles distinct bytes into every subresource, checks each element's
/// address is its own, then detiles (whole and in bands) and compares.
fn check(l: &Layout) -> Result<(), String> {
    let d = l.desc;
    let n = d.element_bytes as usize;
    let mut tiled = vec![0u8; l.size as usize];
    let mut owner = vec![u32::MAX; l.size as usize];
    let slices = if d.dimension == Dimension::Tex3d {
        1
    } else {
        d.depth
    };
    let mut id = 0u32;
    for slice in 0..slices {
        for mip in 0..d.mips {
            let m = l.mips[mip as usize];
            let active = m.width as usize * n;
            let row = active + 5;
            let plane = row * m.height as usize + 3;
            let linear: Vec<u8> = (0..plane * m.depth as usize)
                .map(|i| {
                    (i as u32)
                        .wrapping_mul(2_654_435_761)
                        .rotate_left(mip + slice) as u8
                })
                .collect();
            l.tile(&mut tiled, mip, slice, &linear, row, plane)
                .map_err(|e| format!("tile mip {mip} slice {slice}: {e:?}"))?;
            for z in 0..m.depth {
                for y in 0..m.height {
                    for x in 0..m.width {
                        let at = l
                            .offset(mip, x, y, slice + z)
                            .map_err(|e| format!("offset: {e:?}"))?
                            as usize;
                        if at + n > owner.len() || !at.is_multiple_of(n) {
                            return Err(format!(
                                "mip {mip} ({x},{y},{z}) at {at:#x}: outside or unaligned"
                            ));
                        }
                        if owner[at] != u32::MAX {
                            return Err(format!("mip {mip} ({x},{y},{z}) at {at:#x}: shared"));
                        }
                        owner[at] = id;
                        id += 1;
                    }
                }
            }
            let mut back = vec![0xa5u8; linear.len()];
            l.detile(&tiled, mip, slice, &mut back, row, plane)
                .map_err(|e| format!("detile: {e:?}"))?;
            for z in 0..m.depth as usize {
                for y in 0..m.height as usize {
                    let start = z * plane + y * row;
                    if back[start..start + active] != linear[start..start + active] {
                        return Err(format!(
                            "mip {mip} slice {slice} row {y} depth {z}: rows differ"
                        ));
                    }
                    if back[start + active..start + row].iter().any(|&b| b != 0xa5) {
                        return Err(format!("mip {mip} row {y}: padding written"));
                    }
                }
            }
            // A 2D subresource detiled in two bands gives the same rows.
            if m.depth == 1 && m.height > 1 {
                let split = m.height / 2;
                let mut banded = vec![0xa5u8; row * m.height as usize];
                let (top, bottom) = banded.split_at_mut(split as usize * row);
                l.detile_rows(&tiled, mip, slice, top, row, 0..split)
                    .map_err(|e| format!("detile_rows top: {e:?}"))?;
                l.detile_rows(&tiled, mip, slice, bottom, row, split..m.height)
                    .map_err(|e| format!("detile_rows bottom: {e:?}"))?;
                for y in 0..m.height as usize {
                    let at = y * row;
                    if banded[at..at + active] != back[at..at + active] {
                        return Err(format!("mip {mip} row {y}: bands differ from whole"));
                    }
                }
            }
        }
    }
    Ok(())
}

#[test]
fn random_layouts_round_trip_and_never_share_an_address() {
    let cases = env("VICTORY_TILE_CASES", 300);
    let seed = env("VICTORY_TILE_SEED", 0x005e_ed0f_7e57);
    let mut checked = 0;
    let mut by_mode = [0u32; 32];
    for case in 0..cases {
        let case_seed = seed ^ case.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        let mut rng = Rng(case_seed);
        let d = random_desc(&mut rng);
        let l = match Layout::new(&d) {
            Ok(l) => l,
            // Descriptions a mode cannot hold, and pipe-XOR addressing
            // not implemented here, are not this test's business.
            Err(Error::Invalid | Error::Unsupported) => continue,
            Err(e) => panic!("case seed {case_seed:#x}: {d:?}: {e:?}"),
        };
        if !l.addressable() {
            continue;
        }
        if let Err(why) = check(&l) {
            panic!("case seed {case_seed:#x} (VICTORY_TILE_SEED={seed}): {d:?}: {why}");
        }
        checked += 1;
        by_mode[d.swizzle.0 as usize] += 1;
    }
    // About a third of draws make an addressable layout (pipe-XOR modes and
    // combinations a mode cannot hold do not); far fewer means the generator
    // has drifted.
    assert!(
        checked * 4 >= cases,
        "only {checked} of {cases} descriptions made a layout"
    );
    for mode in [0, 1, 2, 5, 6, 9, 10] {
        assert!(
            cases < 100 || by_mode[mode] > 0,
            "swizzle mode {mode} never checked: {by_mode:?}"
        );
    }
}
