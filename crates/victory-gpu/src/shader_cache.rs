//! Translated shaders kept on disk (decision 0003): under the configured
//! cache folder, keyed by the container's content and by a deterministic
//! translator + schema fingerprint (`victory-shader`'s compile-time source
//! hash, this module's serialization schema version, and the active loop
//! limit). That way translator edits invalidate cached entries automatically
//! while unrelated crate rebuilds (`victory-hle`, `victory-gpu`,
//! `victory-loader`, `diag` vs `release`) keep the cache warm. Entries go
//! through `victory-pipeline`'s content-addressed cache, which writes them
//! atomically and deduplicates concurrent in-flight translations; an entry
//! that does not decode cleanly is translated again.
//!
//! The entry is our own encoding of [`HostShader`]: a field added to it
//! must be added to `encode` and `decode` and `CACHE_SCHEMA_VERSION`
//! bumped.

use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

pub use victory_pipeline::Cache;
use victory_pipeline::Converter;
use victory_shader::emit::{HostShader, StaticSampler, TableView, Target, UserData};
use victory_shader::{HwStage, RootType};

static CACHE: OnceLock<Cache> = OnceLock::new();
static HITS: AtomicU64 = AtomicU64::new(0);
static MISSES: AtomicU64 = AtomicU64::new(0);

/// Serialization schema version for [`encode`] / [`decode`]. Bump whenever
/// [`HostShader`]'s on-disk format changes.
const CACHE_SCHEMA_VERSION: u32 = 1;

/// Opens a shader cache at `dir` (outside the repository), for offline AOT
/// warming (`victory translate --write-cache`) or custom callers.
pub fn open(dir: PathBuf) -> anyhow::Result<Cache> {
    Cache::open(dir)
}

/// Keeps translated shaders in `dir` from now on.
pub fn use_shader_cache(dir: PathBuf) {
    match Cache::open(dir) {
        Ok(cache) => {
            let _ = CACHE.set(cache);
        }
        Err(e) => tracing::warn!(error = %e, "no shader cache"),
    }
}

/// Stores an already-translated [`HostShader`] for `container` into `cache`
/// under the current translator fingerprint, for a host that runs `target`'s
/// wave sizes. Returns `true` if a new entry was written, `false` if it was
/// already present.
pub fn store(
    cache: &Cache,
    container: &[u8],
    target: Target,
    shader: &HostShader,
) -> anyhow::Result<bool> {
    store_with_build(cache, &key(build(), target), container, shader)
}

fn store_with_build(
    cache: &Cache,
    build: &[u8; 32],
    container: &[u8],
    shader: &HostShader,
) -> anyhow::Result<bool> {
    let input = [&build[..], container].concat();
    let encoded = encode(shader);
    let (_, written) = cache.put(&KeySpec, &input, &encoded)?;
    Ok(written)
}

/// Shaders found in the cache and translated since the start.
pub(crate) fn counts() -> (u64, u64) {
    (HITS.load(Ordering::Relaxed), MISSES.load(Ordering::Relaxed))
}

/// Deterministic translator + cache-format fingerprint: hashes
/// `victory-shader`'s compile-time source digest, the encoding schema version,
/// and the active loop and spin-wait limits (which
/// `VICTORY_SHADER_LOOP_LIMIT` and `VICTORY_SHADER_SPIN_LIMIT` change at run
/// time).
fn build() -> &'static [u8; 32] {
    static BUILD: OnceLock<[u8; 32]> = OnceLock::new();
    BUILD.get_or_init(|| {
        let limit = victory_shader::emit::loop_limit();
        if limit != victory_shader::emit::DEFAULT_LOOP_LIMIT {
            tracing::warn!(
                limit,
                "VICTORY_SHADER_LOOP_LIMIT: translated loops stop sooner"
            );
        }
        let spin = victory_shader::emit::spin_limit();
        if spin != victory_shader::emit::DEFAULT_SPIN_LIMIT {
            tracing::warn!(
                spin,
                "VICTORY_SHADER_SPIN_LIMIT: spin-wait loops stop at this count"
            );
        }
        build_fingerprint(victory_shader::SOURCE_HASH, CACHE_SCHEMA_VERSION, limit, spin)
    })
}

fn build_fingerprint(source_hash: &str, schema_version: u32, loop_limit: u32, spin_limit: u32) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(MAGIC);
    h.update(&schema_version.to_le_bytes());
    h.update(source_hash.as_bytes());
    h.update(&[0]);
    h.update(&loop_limit.to_le_bytes());
    h.update(&spin_limit.to_le_bytes());
    *h.finalize().as_bytes()
}

/// The translation of a console shader container for a host that runs
/// `target`'s wave sizes, from the cache when there is one.
pub(crate) fn host_shader(container: &[u8], target: Target) -> Result<HostShader, String> {
    let translate =
        |c: &[u8]| victory_shader::emit::translate_for(c, target).map_err(|e| e.to_string());
    match CACHE.get() {
        Some(cache) => cached(cache, &key(build(), target), container, translate),
        None => translate(container),
    }
}

/// The part of an entry's key before the container: the build, and the
/// host's wave sizes (a translation for one host's waves is not another's).
fn key(build: &[u8; 32], target: Target) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(build);
    h.update(&target.key());
    *h.finalize().as_bytes()
}

/// Converter identity shared by [`Translate`] and [`store_with_build`] so
/// offline cache warming and runtime lookups compute identical keys.
struct KeySpec;

impl Converter for KeySpec {
    fn id(&self) -> &'static str {
        "shader.dxil"
    }
    fn version(&self) -> u32 {
        // The build fingerprint in the input does the versioning; bumped
        // with MAGIC when the encoded HostShader layout changes.
        3
    }
    fn convert(&self, _input: &[u8]) -> anyhow::Result<Vec<u8>> {
        unreachable!("KeySpec is only used for Cache::put key computation")
    }
}

/// The cache entry's converter: the build and the container in, the
/// encoded translation out.
struct Translate<F> {
    translate: F,
    /// The translation it ran worked: a cache error after that is the
    /// cache's (it could not keep the entry), not the shader's.
    translated: AtomicBool,
}

impl<F: Fn(&[u8]) -> Result<HostShader, String> + Send + Sync> Converter for Translate<F> {
    fn id(&self) -> &'static str {
        KeySpec.id()
    }
    fn version(&self) -> u32 {
        KeySpec.version()
    }
    fn convert(&self, input: &[u8]) -> anyhow::Result<Vec<u8>> {
        let shader = (self.translate)(&input[32..]).map_err(anyhow::Error::msg)?;
        self.translated.store(true, Ordering::Relaxed);
        Ok(encode(&shader))
    }
}

fn cached(
    cache: &Cache,
    build: &[u8; 32],
    container: &[u8],
    translate: impl Fn(&[u8]) -> Result<HostShader, String> + Send + Sync,
) -> Result<HostShader, String> {
    let input = [&build[..], container].concat();
    let converter = Translate {
        translate,
        translated: AtomicBool::new(false),
    };
    for _ in 0..2 {
        let (path, bytes, from_cache) = match cache.get_or_convert_bytes(&converter, &input) {
            Ok(entry) => entry,
            // A full disk, a file in the way, a scanner holding the new
            // entry: the shader is still good, only not kept.
            Err(e) if converter.translated.load(Ordering::Relaxed) => {
                tracing::warn!(error = format!("{e:#}"), "shader cache entry not written");
                break;
            }
            Err(e) => return Err(format!("{e:#}")),
        };
        if let Some(shader) = decode(&bytes) {
            let (counter, what) = if from_cache {
                (&HITS, "from the cache")
            } else {
                (&MISSES, "translated and cached")
            };
            counter.fetch_add(1, Ordering::Relaxed);
            tracing::debug!(path = %path.display(), "shader {what}");
            return Ok(shader);
        }
        tracing::debug!(path = %path.display(), "unreadable shader cache entry; translating again");
        let _ = std::fs::remove_file(&path);
    }
    MISSES.fetch_add(1, Ordering::Relaxed);
    (converter.translate)(container)
}

const MAGIC: &[u8; 4] = b"VSH3";
const STAGES: [HwStage; 8] = [
    HwStage::Ls,
    HwStage::Hs,
    HwStage::Es,
    HwStage::Gs,
    HwStage::Vs,
    HwStage::Ps,
    HwStage::Ms,
    HwStage::Cs,
];

fn root_code(r: RootType) -> u32 {
    match r {
        RootType::Constants => 0,
        RootType::Descriptor => 1,
        RootType::Table => 2,
        RootType::ImmediateSampler => 3,
        RootType::ResourceHeap => 4,
        RootType::SamplerHeap => 5,
        RootType::Other(x) => 6 + x,
    }
}

fn root_type(code: u32) -> RootType {
    match code {
        0 => RootType::Constants,
        1 => RootType::Descriptor,
        2 => RootType::Table,
        3 => RootType::ImmediateSampler,
        4 => RootType::ResourceHeap,
        5 => RootType::SamplerHeap,
        x => RootType::Other(x - 6),
    }
}

/// Little-endian words and length-prefixed lists.
fn encode(s: &HostShader) -> Vec<u8> {
    let mut out = MAGIC.to_vec();
    let word = |out: &mut Vec<u8>, v: u32| out.extend(v.to_le_bytes());
    let stage = STAGES.iter().position(|&x| x == s.stage).unwrap_or(0) as u32;
    word(&mut out, stage);
    word(&mut out, s.dxil.len() as u32);
    out.extend(&s.dxil);
    word(&mut out, s.user_data.len() as u32);
    for u in &s.user_data {
        for v in [u.index, u32::from(u.slot), root_code(u.root), u.dword] {
            word(&mut out, v);
        }
    }
    word(&mut out, s.samplers.len() as u32);
    for sampler in &s.samplers {
        for &v in sampler {
            word(&mut out, v);
        }
    }
    word(&mut out, s.static_samplers.len() as u32);
    for t in &s.static_samplers {
        let [u, v, w] = t.address;
        let words = [
            t.filter,
            u,
            v,
            w,
            t.mip_lod_bias.to_bits(),
            t.max_anisotropy,
            t.comparison,
            t.border,
            t.min_lod.to_bits(),
            t.max_lod.to_bits(),
        ];
        for v in words {
            word(&mut out, v);
        }
    }
    for slots in [&s.root_cbvs, &s.root_srvs] {
        word(&mut out, slots.len() as u32);
        out.extend(slots);
    }
    word(&mut out, s.table_views.len() as u32);
    for v in &s.table_views {
        for w in [u32::from(v.slot), v.dword, v.props.0, v.props.1] {
            word(&mut out, w);
        }
    }
    word(&mut out, u32::from(s.stride_table));
    out
}

fn decode(bytes: &[u8]) -> Option<HostShader> {
    struct Reader<'a>(&'a [u8]);
    impl<'a> Reader<'a> {
        fn take(&mut self, n: usize) -> Option<&'a [u8]> {
            let (head, rest) = self.0.split_at_checked(n)?;
            self.0 = rest;
            Some(head)
        }
        fn word(&mut self) -> Option<u32> {
            Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
        }
        fn list(&mut self) -> Option<Vec<u8>> {
            let n = self.word()? as usize;
            Some(self.take(n)?.to_vec())
        }
    }
    let mut r = Reader(bytes);
    if r.take(4)? != MAGIC {
        return None;
    }
    let stage = *STAGES.get(r.word()? as usize)?;
    let dxil = r.list()?;
    let n = r.word()? as usize;
    let mut user_data = Vec::with_capacity(n.min(256));
    for _ in 0..n {
        user_data.push(UserData {
            index: r.word()?,
            slot: u8::try_from(r.word()?).ok()?,
            root: root_type(r.word()?),
            dword: r.word()?,
        });
    }
    let n = r.word()? as usize;
    let mut samplers = Vec::with_capacity(n.min(256));
    for _ in 0..n {
        samplers.push([r.word()?, r.word()?, r.word()?, r.word()?]);
    }
    let n = r.word()? as usize;
    let mut static_samplers = Vec::with_capacity(n.min(256));
    for _ in 0..n {
        static_samplers.push(StaticSampler {
            filter: r.word()?,
            address: [r.word()?, r.word()?, r.word()?],
            mip_lod_bias: f32::from_bits(r.word()?),
            max_anisotropy: r.word()?,
            comparison: r.word()?,
            border: r.word()?,
            min_lod: f32::from_bits(r.word()?),
            max_lod: f32::from_bits(r.word()?),
        });
    }
    let root_cbvs = r.list()?;
    let root_srvs = r.list()?;
    let n = r.word()? as usize;
    let mut table_views = Vec::with_capacity(n.min(256));
    for _ in 0..n {
        table_views.push(TableView {
            slot: u8::try_from(r.word()?).ok()?,
            dword: r.word()?,
            props: (r.word()?, r.word()?),
        });
    }
    let stride_table = r.word()? != 0;
    r.0.is_empty().then_some(HostShader {
        stage,
        dxil,
        user_data,
        samplers,
        static_samplers,
        root_cbvs,
        root_srvs,
        table_views,
        stride_table,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shader() -> HostShader {
        HostShader {
            stage: HwStage::Ps,
            dxil: b"DXBC and the rest".to_vec(),
            user_data: vec![UserData {
                index: 8,
                slot: 1,
                root: RootType::Table,
                dword: 0,
            }],
            samplers: vec![[0x400, 0xc0_0000, 0x18f0_0000, 0]],
            static_samplers: vec![StaticSampler::from_words([
                0x400,
                0xc0_0000,
                0x18f0_0000,
                0,
            ])],
            root_cbvs: vec![2],
            root_srvs: vec![0, 3],
            table_views: vec![
                TableView {
                    slot: 1,
                    dword: 8,
                    props: (11 | 0x1000, 0),
                },
                TableView {
                    slot: 1,
                    dword: 16,
                    props: (12 | 0x1000, 32),
                },
            ],
            stride_table: true,
        }
    }

    #[test]
    fn translations_come_back_from_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path()).unwrap();
        let build = [7u8; 32];
        let container = b"a console container".as_slice();
        let runs = AtomicU64::new(0);
        let translate = |_: &[u8]| {
            runs.fetch_add(1, Ordering::Relaxed);
            Ok(shader())
        };
        assert_eq!(cached(&cache, &build, container, translate), Ok(shader()));
        assert_eq!(cached(&cache, &build, container, translate), Ok(shader()));
        assert_eq!(runs.load(Ordering::Relaxed), 1, "the second came from disk");

        // Another build translates again.
        assert_eq!(cached(&cache, &[8; 32], container, translate), Ok(shader()));
        assert_eq!(runs.load(Ordering::Relaxed), 2);

        // A damaged entry is a miss, translated and stored again.
        for e in walk(dir.path()) {
            std::fs::write(e, b"VSH3 broken").unwrap();
        }
        assert_eq!(cached(&cache, &build, container, translate), Ok(shader()));
        assert_eq!(runs.load(Ordering::Relaxed), 3);
        assert_eq!(cached(&cache, &build, container, translate), Ok(shader()));
        assert_eq!(runs.load(Ordering::Relaxed), 3);

        // Failures are not stored.
        let fail = |_: &[u8]| Err::<HostShader, _>("unsupported".to_owned());
        assert!(cached(&cache, &build, b"other", fail).is_err());
    }

    #[test]
    fn a_cache_that_cannot_keep_an_entry_still_gives_the_translation() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path()).unwrap();
        // A file where every entry's folder would go: nothing can be written.
        for shard in 0..=0xff_u8 {
            std::fs::write(dir.path().join(format!("{shard:02x}")), b"").unwrap();
        }
        let translate = |_: &[u8]| Ok(shader());
        let container = b"a console container".as_slice();
        assert_eq!(cached(&cache, &[7; 32], container, translate), Ok(shader()));
        // A shader that does not translate still fails, with its own error.
        let fail = |_: &[u8]| Err::<HostShader, _>("unsupported".to_owned());
        let error = cached(&cache, &[7; 32], b"other", fail).unwrap_err();
        assert!(error.contains("unsupported"), "{error}");
    }

    #[test]
    fn keys_tell_hosts_wave_sizes_apart() {
        let build = [7u8; 32];
        let nvidia = Target {
            lanes_min: 32,
            lanes_max: 32,
        };
        let rdna = Target {
            lanes_min: 32,
            lanes_max: 64,
        };
        assert_eq!(key(&build, nvidia), key(&build, nvidia));
        assert_ne!(key(&build, nvidia), key(&build, rdna));
        assert_ne!(key(&build, nvidia), key(&[8; 32], nvidia));
    }

    fn walk(dir: &std::path::Path) -> Vec<PathBuf> {
        let mut files = Vec::new();
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            if e.path().is_dir() {
                files.extend(walk(&e.path()));
            } else {
                files.push(e.path());
            }
        }
        files
    }

    #[test]
    fn entries_round_trip_and_reject_damage() {
        let bytes = encode(&shader());
        assert_eq!(decode(&bytes), Some(shader()));
        assert_eq!(decode(&bytes[..bytes.len() - 1]), None);
        assert_eq!(decode(&[bytes.as_slice(), &[0]].concat()), None);
        assert_eq!(decode(b"VSH0"), None);
        assert_eq!(decode(b"VSH1"), None);
        assert_eq!(decode(b"VSH2"), None);
    }

    #[test]
    fn build_fingerprint_is_deterministic_and_sensitive() {
        let base = build_fingerprint(victory_shader::SOURCE_HASH, CACHE_SCHEMA_VERSION, 1 << 20, 1 << 12);
        assert_eq!(
            base,
            build_fingerprint(victory_shader::SOURCE_HASH, CACHE_SCHEMA_VERSION, 1 << 20, 1 << 12),
            "same inputs produce identical key"
        );
        assert_eq!(victory_shader::SOURCE_HASH.len(), 64, "BLAKE3 hex digest");
        assert_ne!(
            base,
            build_fingerprint("different-source-hash", CACHE_SCHEMA_VERSION, 1 << 20, 1 << 12),
            "source hash changes invalidate key"
        );
        assert_ne!(
            base,
            build_fingerprint(
                victory_shader::SOURCE_HASH,
                CACHE_SCHEMA_VERSION + 1,
                1 << 20,
                1 << 12
            ),
            "schema version bump invalidates key"
        );
        assert_ne!(
            base,
            build_fingerprint(victory_shader::SOURCE_HASH, CACHE_SCHEMA_VERSION, 1024, 1 << 12),
            "loop limit override invalidates key"
        );
        assert_ne!(
            base,
            build_fingerprint(victory_shader::SOURCE_HASH, CACHE_SCHEMA_VERSION, 1 << 20, 64),
            "spin limit override invalidates key"
        );
    }

    #[test]
    fn store_warms_cache_for_subsequent_runtime_lookup() {
        let dir = tempfile::tempdir().unwrap();
        let cache = open(dir.path().join("shaders")).unwrap();
        let container = b"prewarmed-container".as_slice();
        let expected = shader();

        assert!(store(&cache, container, Target::ANY, &expected).unwrap());
        assert!(!store(&cache, container, Target::ANY, &expected).unwrap());

        let runs = AtomicU64::new(0);
        let got = cached(&cache, &key(build(), Target::ANY), container, |_| {
            runs.fetch_add(1, Ordering::Relaxed);
            Ok(expected.clone())
        });
        assert_eq!(got, Ok(expected));
        assert_eq!(
            runs.load(Ordering::Relaxed),
            0,
            "runtime lookup hit the pre-warmed entry without translating"
        );
    }

    #[test]
    fn concurrent_shader_cache_lookups_translate_once() {
        use std::sync::Barrier;
        use std::time::Duration;

        let dir = tempfile::tempdir().unwrap();
        let cache = open(dir.path().join("shaders")).unwrap();
        let container = b"shared-shader-container".as_slice();
        let runs = AtomicU64::new(0);
        let threads = 6;
        let barrier = Barrier::new(threads);

        std::thread::scope(|s| {
            let handles: Vec<_> = (0..threads)
                .map(|_| {
                    let cache = cache.clone();
                    let runs = &runs;
                    let barrier = &barrier;
                    s.spawn(move || {
                        barrier.wait();
                        cached(&cache, build(), container, |_| {
                            runs.fetch_add(1, Ordering::SeqCst);
                            std::thread::sleep(Duration::from_millis(40));
                            Ok(shader())
                        })
                    })
                })
                .collect();
            for h in handles {
                assert_eq!(h.join().unwrap(), Ok(shader()));
            }
        });
        assert_eq!(
            runs.load(Ordering::SeqCst),
            1,
            "concurrent PSO workers deduplicated onto a single translation"
        );
    }
}
