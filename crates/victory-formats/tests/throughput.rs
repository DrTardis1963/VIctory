//! Throughput of the texture copies and the XMA decoder on synthetic data
//! (no game data). Benchmarks, not tests: they print one `BENCH <name>
//! <value> <unit>` line each for `cargo xtask bench`, which compares them
//! with a saved run. By hand:
//! `cargo test --release -p victory-formats --test throughput -- --ignored --nocapture`.

use std::hint::black_box;
use std::time::Instant;

use victory_formats::texture::{Desc, Dimension, Layout, Swizzle};
use victory_formats::xma::Decoder;

/// The fastest of `batches` timings of `runs` calls, in seconds per call:
/// the minimum is the least disturbed by other work on the machine.
fn time(batches: u32, runs: u32, mut f: impl FnMut()) -> f64 {
    f();
    (0..batches)
        .map(|_| {
            let started = Instant::now();
            for _ in 0..runs {
                f();
            }
            started.elapsed().as_secs_f64() / f64::from(runs)
        })
        .fold(f64::INFINITY, f64::min)
}

#[test]
#[ignore = "benchmark"]
fn texture_throughput() {
    let cases = [
        ("64kb_d_2048_rgba8", 2048, 2048, 4, 1, Swizzle::SW_64KB_D),
        ("64kb_s_1024_rgba16f", 1024, 1024, 8, 1, Swizzle::SW_64KB_S),
        ("4kb_s_512_r8", 512, 512, 1, 1, Swizzle::SW_4KB_S),
        ("64kb_s_bc7_2048", 2048, 2048, 16, 4, Swizzle::SW_64KB_S),
        ("linear_1024_rgba8", 1024, 1024, 4, 1, Swizzle::LINEAR),
    ];
    for (name, width, height, element_bytes, block, swizzle) in cases {
        let desc = Desc {
            dimension: Dimension::Tex2d,
            width,
            height,
            depth: 1,
            mips: 1,
            element_bytes,
            block,
            samples: 1,
            swizzle,
        };
        let l = Layout::new(&desc).unwrap();
        let m = l.mips[0];
        let pitch = m.width as usize * element_bytes as usize;
        let bytes = pitch * m.height as usize;
        let mut tiled: Vec<u8> = (0..l.size as usize).map(|i| (i * 31) as u8).collect();
        let mut linear = vec![0u8; bytes];
        let secs = time(5, 4, || {
            l.detile(black_box(&tiled), 0, 0, &mut linear, pitch, bytes)
                .unwrap();
        });
        println!("BENCH detile/{name} {:.1} MB/s", bytes as f64 / secs / 1e6);
        let secs = time(5, 4, || {
            l.tile(&mut tiled, 0, 0, black_box(&linear), pitch, bytes)
                .unwrap();
        });
        println!("BENCH tile/{name} {:.1} MB/s", bytes as f64 / secs / 1e6);
    }
}

/// XMA frames of noise: every frame decodes to a frame of samples whether
/// or not it parses, so this times the decoder's worst-case paths rather
/// than music; still a fair regression signal.
#[test]
#[ignore = "benchmark"]
fn xma_throughput() {
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut next = || {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        state.wrapping_mul(0x2545_F491_4F6C_DD1D)
    };
    let frames: Vec<Vec<u8>> = (0..256)
        .map(|_| (0..512).map(|_| next() as u8).collect())
        .collect();
    for channels in [1, 2] {
        let mut decoder = Decoder::new(channels, 48_000);
        let mut pcm = Vec::with_capacity(frames.len() * 512 * channels);
        let secs = time(5, 2, || {
            pcm.clear();
            decoder.reset();
            for f in &frames {
                let _ = decoder.decode(black_box(f), f.len() * 8, &mut pcm);
            }
        });
        println!(
            "BENCH xma/noise_{channels}ch {:.1} us/frame",
            secs * 1e6 / frames.len() as f64
        );
    }
}
