//! Shader and pipeline cache counters, for chasing frame-time hitches.
//!
//! A frame that waits for a translation or a host pipeline compile stalls
//! for as long as that takes, so a run's log says how many of each it did,
//! how long they took in all and at most, and how often the caches on disk
//! and in memory spared them. The counters are relaxed atomics: they only
//! ever go up and nothing orders other memory by them, so counting costs
//! one uncontended add where the work it counts costs milliseconds.
//!
//! [`line`] is logged beside the per-frame line, at the same cadence, as
//! `key=value` pairs that `cargo xtask harness analyze` reads into its
//! report's `cache` section; [`report_at_exit`] prints [`summary`] when the
//! process ends.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

/// How many times something ran, for how long in all and at most.
pub struct Timing {
    count: AtomicU64,
    total_us: AtomicU64,
    max_us: AtomicU64,
}

impl Timing {
    pub const fn new() -> Self {
        Self {
            count: AtomicU64::new(0),
            total_us: AtomicU64::new(0),
            max_us: AtomicU64::new(0),
        }
    }

    pub fn add(&self, took: Duration) {
        let us = took.as_micros().min(u128::from(u64::MAX)) as u64;
        self.count.fetch_add(1, Relaxed);
        self.total_us.fetch_add(us, Relaxed);
        self.max_us.fetch_max(us, Relaxed);
    }

    /// `(count, total ms, max ms)`.
    pub fn read(&self) -> (u64, f64, f64) {
        (
            self.count.load(Relaxed),
            self.total_us.load(Relaxed) as f64 / 1e3,
            self.max_us.load(Relaxed) as f64 / 1e3,
        )
    }
}

impl Default for Timing {
    fn default() -> Self {
        Self::new()
    }
}

/// What kind of host pipeline was made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Graphics,
    Compute,
    Mesh,
}

/// Shader containers the title handed over that the table had already
/// (one translation serves them), and new ones.
static MEMORY: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
/// Translations run in this process (not served from the disk cache).
static TRANSLATIONS: Timing = Timing::new();
/// Host pipelines made or loaded, by [`Kind`], and those the host refused.
static PIPELINES: [Timing; 3] = [const { Timing::new() }; 3];
static FAILED: AtomicU64 = AtomicU64::new(0);
/// Host pipelines the pipeline library loaded, and those it did not have
/// (compiled, then stored).
static LIBRARY: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// A shader container was looked up in the table: `new` if it was not
/// there yet.
pub fn shader_seen(new: bool) {
    MEMORY[usize::from(new)].fetch_add(1, Relaxed);
}

/// A translation ran for `took`.
pub fn translated(took: Duration) {
    TRANSLATIONS.add(took);
}

/// A host pipeline of `kind` was asked for at `began`; `made` if the host
/// gave one.
pub fn pipeline_made(kind: Kind, began: Instant, made: bool) {
    PIPELINES[kind as usize].add(began.elapsed());
    if !made {
        FAILED.fetch_add(1, Relaxed);
    }
}

/// The pipeline library had the pipeline asked for (`hit`), or not.
pub fn library_lookup(hit: bool) {
    LIBRARY[usize::from(!hit)].fetch_add(1, Relaxed);
}

/// Every counter at one moment.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    pub disk_hits: u64,
    pub disk_misses: u64,
    pub memory_hits: u64,
    pub memory_misses: u64,
    /// `(count, total ms, max ms)`.
    pub translations: (u64, f64, f64),
    /// Graphics, compute, mesh: `(count, total ms, max ms)`.
    pub pipelines: [(u64, f64, f64); 3],
    pub pipeline_failures: u64,
    pub library_hits: u64,
    pub library_misses: u64,
}

/// The counters now.
pub fn snapshot() -> Snapshot {
    #[cfg(windows)]
    let (disk_hits, disk_misses) = crate::shader_cache::counts();
    #[cfg(not(windows))]
    let (disk_hits, disk_misses) = (0, 0);
    Snapshot {
        disk_hits,
        disk_misses,
        memory_hits: MEMORY[0].load(Relaxed),
        memory_misses: MEMORY[1].load(Relaxed),
        translations: TRANSLATIONS.read(),
        pipelines: [0, 1, 2].map(|i| PIPELINES[i].read()),
        pipeline_failures: FAILED.load(Relaxed),
        library_hits: LIBRARY[0].load(Relaxed),
        library_misses: LIBRARY[1].load(Relaxed),
    }
}

impl Snapshot {
    /// The log clause: `cache:` then `key=value` pairs, totals since the
    /// start. The keys are what the harness report's `cache` section names.
    pub fn line(&self) -> String {
        let mut s = format!(
            "cache: shader_disk_hits={} shader_disk_misses={} shader_memory_hits={} shader_memory_misses={} \
             translations={} translate_ms={:.1} translate_max_ms={:.1}",
            self.disk_hits,
            self.disk_misses,
            self.memory_hits,
            self.memory_misses,
            self.translations.0,
            self.translations.1,
            self.translations.2,
        );
        for (name, (n, total, max)) in ["graphics", "compute", "mesh"].iter().zip(self.pipelines) {
            s.push_str(&format!(
                " {name}={n} {name}_ms={total:.1} {name}_max_ms={max:.1}"
            ));
        }
        s.push_str(&format!(
            " pipeline_failures={} library_hits={} library_misses={}",
            self.pipeline_failures, self.library_hits, self.library_misses
        ));
        s
    }

    /// A few lines for people, for the end of a run.
    pub fn summary(&self) -> String {
        let (t, t_ms, t_max) = self.translations;
        let mut s = format!(
            "shader cache   {} from disk, {} translated and cached; {} containers shared in memory, {} new\n\
             translations   {t} at run time, {t_ms:.1} ms in all, the longest {t_max:.1} ms\n",
            self.disk_hits, self.disk_misses, self.memory_hits, self.memory_misses
        );
        for (name, (n, total, max)) in ["graphics", "compute", "mesh"].iter().zip(self.pipelines) {
            s.push_str(&format!(
                "host pipelines {name}: {n}, {total:.1} ms in all, the longest {max:.1} ms\n"
            ));
        }
        s.push_str(&format!(
            "host pipelines {} refused; the pipeline library loaded {} and did not have {}",
            self.pipeline_failures, self.library_hits, self.library_misses
        ));
        s
    }
}

/// The log clause for the counters now ([`Snapshot::line`]).
pub fn line() -> String {
    snapshot().line()
}

/// Logs [`line`]; called beside the per-frame line.
pub fn log() {
    tracing::info!("{}", line());
    #[cfg(windows)]
    if AT_EXIT.load(Relaxed) {
        register_exit();
    }
}

/// The counters as [`Snapshot::summary`] puts them.
pub fn summary() -> String {
    snapshot().summary()
}

static AT_EXIT: AtomicBool = AtomicBool::new(false);

/// Prints [`summary`] on stdout when the process ends (`victory run`).
///
/// The printing is registered with the C runtime's exit functions at the
/// first [`log`], not here: exit functions run last-registered first, and
/// the loader's, registered when the title's images attach, ends every
/// other thread. One registered before it would print after that, when an
/// ended thread may hold stdout's lock.
pub fn report_at_exit() {
    AT_EXIT.store(true, Relaxed);
}

#[cfg(windows)]
fn register_exit() {
    unsafe extern "C" {
        fn atexit(f: extern "C" fn()) -> i32;
    }
    extern "C" fn print() {
        println!("{}", summary());
    }
    static ONCE: std::sync::Once = std::sync::Once::new();
    // SAFETY: registers a function with the host C runtime's exit table.
    ONCE.call_once(|| unsafe {
        atexit(print);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_timing_keeps_the_count_the_total_and_the_longest() {
        let t = Timing::new();
        t.add(Duration::from_micros(1500));
        t.add(Duration::from_micros(4000));
        t.add(Duration::from_micros(500));
        assert_eq!(t.read(), (3, 6.0, 4.0));
    }

    #[test]
    fn counters_only_go_up() {
        let before = snapshot();
        shader_seen(true);
        shader_seen(false);
        shader_seen(false);
        translated(Duration::from_millis(2));
        pipeline_made(Kind::Mesh, Instant::now(), false);
        library_lookup(true);
        library_lookup(false);
        let after = snapshot();
        // Other tests may count too: at least these.
        assert!(after.memory_misses > before.memory_misses);
        assert!(after.memory_hits >= before.memory_hits + 2);
        assert!(after.translations.0 > before.translations.0);
        assert!(after.translations.2 >= 2.0);
        assert!(after.pipelines[Kind::Mesh as usize].0 > before.pipelines[Kind::Mesh as usize].0);
        assert!(after.pipeline_failures > before.pipeline_failures);
        assert!(after.library_hits > before.library_hits);
        assert!(after.library_misses > before.library_misses);
    }

    #[test]
    fn the_log_clause_names_every_counter_once() {
        let s = Snapshot {
            disk_hits: 812,
            disk_misses: 3,
            memory_hits: 1450,
            memory_misses: 815,
            translations: (3, 45.14, 30.2),
            pipelines: [(120, 2400.5, 80.1), (40, 300.0, 12.5), (2, 10.0, 6.0)],
            pipeline_failures: 1,
            library_hits: 150,
            library_misses: 12,
        };
        assert_eq!(
            s.line(),
            "cache: shader_disk_hits=812 shader_disk_misses=3 shader_memory_hits=1450 \
             shader_memory_misses=815 translations=3 translate_ms=45.1 translate_max_ms=30.2 \
             graphics=120 graphics_ms=2400.5 graphics_max_ms=80.1 compute=40 compute_ms=300.0 \
             compute_max_ms=12.5 mesh=2 mesh_ms=10.0 mesh_max_ms=6.0 pipeline_failures=1 \
             library_hits=150 library_misses=12"
        );
        assert!(
            s.summary()
                .contains("mesh: 2, 10.0 ms in all, the longest 6.0 ms")
        );
    }
}
