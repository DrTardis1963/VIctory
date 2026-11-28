//! The GPU command-stream recorder: which D3D12.X calls a few frames make,
//! for `cargo xtask gpu-view`.
//!
//! `VICTORY_GPU_RECORD=<dir>` (outside the repository) records
//! `VICTORY_GPU_RECORD_FRAMES` frames (3 if unset) into
//! `<dir>/frame-<n>.ndjson`, one JSON record per line. They start at frame
//! `VICTORY_GPU_RECORD_AT`, or else after the first frame that made 1,000
//! draws or more: menus and loading screens make a few dozen, the world
//! thousands, and it is the world's frames that miss 30 fps.
//!
//! Only calls are recorded, never what they point at: per command list its
//! commands in order (draws with their vertex, index and instance counts,
//! dispatches, barriers, copies, clears, resolves, pipelines with their
//! shaders' hashes, render target changes, root arguments as a count), and
//! per queue its submissions, fence signals and waits, each with the CPU
//! time in microseconds since the recorder started.
//!
//! Every hooked call first makes one relaxed load of [`ON`], which is false
//! unless a recording was asked for and is not over: a run that does not
//! record pays that and nothing else. While it waits for its first frame it
//! only counts draws; it takes a lock per call only inside the frames it
//! writes.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, Once};
use std::time::Instant;

use crate::object::This;

/// A recording was asked for and is not over: the one check hooks make.
static ON: AtomicBool = AtomicBool::new(false);
/// Inside the frames written: calls go to the file, not only the count.
static WRITING: AtomicBool = AtomicBool::new(false);
/// Draws in the current frame while waiting for the first frame written.
static DRAWS: AtomicU64 = AtomicU64::new(0);
static RECORDER: Mutex<Option<Recorder>> = Mutex::new(None);

/// A frame with this many draws starts the recording when no frame number
/// is given.
const MIN_DRAWS: u64 = 1000;

/// A command-list call, as recorded.
pub(crate) enum Cmd {
    /// The list was reset: a new recording into it begins.
    Begin {
        kind: u32,
    },
    Close,
    Draw {
        vertices: u32,
        instances: u32,
    },
    DrawIndexed {
        indices: u32,
        instances: u32,
    },
    DrawMesh {
        groups: u32,
    },
    Dispatch {
        x: u32,
        y: u32,
        z: u32,
    },
    Indirect {
        max: u32,
    },
    Barrier {
        count: u32,
    },
    Copy(&'static str),
    Clear(&'static str),
    Resolve,
    Pipeline(This),
    /// A root signature, table, constant or view set; runs of them are
    /// written as one record with their count.
    Root,
    /// Render targets set: how many, the first's handle, the depth's.
    Targets {
        count: u32,
        first: u64,
        depth: u64,
    },
}

impl Cmd {
    fn is_draw(&self) -> bool {
        matches!(
            self,
            Cmd::Draw { .. }
                | Cmd::DrawIndexed { .. }
                | Cmd::DrawMesh { .. }
                | Cmd::Indirect { .. }
        )
    }

    /// The record's fields after `"op":`.
    fn fields(&self) -> String {
        match self {
            Cmd::Begin { kind } => format!("\"begin\",\"kind\":{kind}"),
            Cmd::Close => "\"close\"".to_owned(),
            Cmd::Draw {
                vertices,
                instances,
            } => {
                format!("\"draw\",\"vertices\":{vertices},\"instances\":{instances}")
            }
            Cmd::DrawIndexed { indices, instances } => {
                format!("\"draw_indexed\",\"indices\":{indices},\"instances\":{instances}")
            }
            Cmd::DrawMesh { groups } => format!("\"draw_mesh\",\"groups\":{groups}"),
            Cmd::Dispatch { x, y, z } => format!("\"dispatch\",\"x\":{x},\"y\":{y},\"z\":{z}"),
            Cmd::Indirect { max } => format!("\"indirect\",\"max\":{max}"),
            Cmd::Barrier { count } => format!("\"barrier\",\"count\":{count}"),
            Cmd::Copy(what) => format!("\"copy\",\"what\":\"{what}\""),
            Cmd::Clear(what) => format!("\"clear\",\"what\":\"{what}\""),
            Cmd::Resolve => "\"resolve\"".to_owned(),
            Cmd::Pipeline(p) => {
                let shaders: Vec<String> = crate::pipeline::shader_names(*p)
                    .iter()
                    .map(|s| format!("\"{s}\""))
                    .collect();
                format!(
                    "\"pipeline\",\"pipeline\":\"{:#x}\",\"shaders\":[{}]",
                    *p as usize,
                    shaders.join(",")
                )
            }
            Cmd::Root => "\"root\",\"count\":1".to_owned(),
            Cmd::Targets {
                count,
                first,
                depth,
            } => format!(
                "\"targets\",\"count\":{count},\"first\":\"{first:#x}\",\"depth\":\"{depth:#x}\""
            ),
        }
    }
}

/// A queue call, as recorded.
pub(crate) enum QueueOp {
    Submit(Vec<usize>),
    Signal { fence: usize, value: u64 },
    Wait { fence: usize, value: u64 },
}

/// Hook for a command-list call: `make` runs only while recording.
#[inline]
pub(crate) fn cmd(list: This, make: impl FnOnce() -> Cmd) {
    if ON.load(Relaxed) {
        note(list as usize, make());
    }
}

/// Hook for a queue call: `make` runs only while recording.
#[inline]
pub(crate) fn queue(queue: This, make: impl FnOnce() -> QueueOp) {
    if ON.load(Relaxed) && WRITING.load(Relaxed) {
        note_queue(queue as usize, crate::queue::kind(queue), make());
    }
}

/// Hook for a present: frame `frame` (the count of presents) has ended.
pub(crate) fn present(frame: u64) {
    static INIT: Once = Once::new();
    INIT.call_once(init);
    if ON.load(Relaxed) {
        presented(frame);
    }
}

/// Which frames are written: `frames` of them from frame `at`, or from the
/// frame after the first with [`MIN_DRAWS`] draws.
#[derive(Debug)]
struct Window {
    at: Option<u64>,
    frames: u64,
    first: Option<u64>,
}

impl Window {
    /// Frame `presented` ended with `draws` draws: the frame beginning now,
    /// if it is written.
    fn after(&mut self, presented: u64, draws: u64) -> Option<u64> {
        let next = presented + 1;
        if self.first.is_none() {
            let begins = match self.at {
                Some(at) => next >= at,
                None => draws >= MIN_DRAWS,
            };
            if begins {
                self.first = Some(next);
            }
        }
        let first = self.first?;
        (next < first + self.frames).then_some(next)
    }

    /// Every frame to write has been written once frame `presented` ended.
    fn over(&self, presented: u64) -> bool {
        self.first
            .is_some_and(|first| presented + 1 >= first + self.frames)
    }
}

struct Recorder {
    dir: PathBuf,
    window: Window,
    began: Instant,
    /// The frame being written and its file.
    out: Option<(u64, std::io::BufWriter<std::fs::File>)>,
    /// Root calls not yet written, per list.
    roots: HashMap<usize, u32>,
    written: u64,
}

fn lock() -> std::sync::MutexGuard<'static, Option<Recorder>> {
    RECORDER.lock().unwrap_or_else(|e| e.into_inner())
}

fn init() {
    let Some(dir) = std::env::var_os("VICTORY_GPU_RECORD").map(PathBuf::from) else {
        return;
    };
    let number = |name: &str| {
        std::env::var(name)
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
    };
    let frames = number("VICTORY_GPU_RECORD_FRAMES").unwrap_or(3).max(1);
    let at = number("VICTORY_GPU_RECORD_AT");
    if let Err(e) = victory_core::ensure_outside_repo(&dir) {
        tracing::warn!(error = %e, "VICTORY_GPU_RECORD: not recording");
        return;
    }
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!(error = %e, dir = %dir.display(), "VICTORY_GPU_RECORD: not recording");
        return;
    }
    tracing::info!(
        dir = %dir.display(),
        frames,
        at = ?at,
        "GPU command recording armed (VICTORY_GPU_RECORD)"
    );
    *lock() = Some(Recorder {
        dir,
        window: Window {
            at,
            frames,
            first: None,
        },
        began: Instant::now(),
        out: None,
        roots: HashMap::new(),
        written: 0,
    });
    ON.store(true, Relaxed);
}

#[cold]
fn presented(frame: u64) {
    let mut guard = lock();
    let Some(rec) = guard.as_mut() else { return };
    let draws = DRAWS.swap(0, Relaxed);
    if let Some((n, _)) = rec.out {
        rec.flush_roots();
        let t = rec.now();
        rec.line(&format!(
            "{{\"type\":\"present\",\"frame\":{n},\"t_us\":{t}}}"
        ));
        if let Some((_, mut out)) = rec.out.take() {
            let _ = out.flush();
        }
        rec.written += 1;
    }
    match rec.window.after(frame, draws) {
        Some(next) => {
            let path = rec.dir.join(format!("frame-{next}.ndjson"));
            match std::fs::File::create(&path) {
                Ok(file) => {
                    rec.out = Some((next, std::io::BufWriter::new(file)));
                    let t = rec.now();
                    rec.line(&format!(
                        "{{\"type\":\"frame\",\"frame\":{next},\"t_us\":{t},\"draws_before\":{draws}}}"
                    ));
                    WRITING.store(true, Relaxed);
                }
                Err(e) => {
                    tracing::warn!(error = %e, path = %path.display(), "GPU recording frame not written")
                }
            }
        }
        None => WRITING.store(false, Relaxed),
    }
    if rec.window.over(frame) {
        WRITING.store(false, Relaxed);
        ON.store(false, Relaxed);
        tracing::info!(
            frames = rec.written,
            dir = %rec.dir.display(),
            "GPU command recording written (cargo xtask gpu-view <dir>)"
        );
        *guard = None;
    }
}

#[cold]
fn note(list: usize, cmd: Cmd) {
    if !WRITING.load(Relaxed) {
        if cmd.is_draw() {
            DRAWS.fetch_add(1, Relaxed);
        }
        return;
    }
    let mut guard = lock();
    let Some(rec) = guard.as_mut() else { return };
    if rec.out.is_none() {
        return;
    }
    if matches!(cmd, Cmd::Root) {
        *rec.roots.entry(list).or_default() += 1;
        return;
    }
    rec.flush_root(list);
    let t = rec.now();
    rec.line(&format!(
        "{{\"type\":\"cmd\",\"t_us\":{t},\"list\":\"{list:#x}\",\"op\":{}}}",
        cmd.fields()
    ));
}

#[cold]
fn note_queue(queue: usize, kind: Option<u32>, op: QueueOp) {
    let mut guard = lock();
    let Some(rec) = guard.as_mut() else { return };
    if rec.out.is_none() {
        return;
    }
    let t = rec.now();
    let kind = kind.map_or("null".to_owned(), |k| k.to_string());
    let head = format!("\"t_us\":{t},\"queue\":\"{queue:#x}\",\"kind\":{kind}");
    let line = match op {
        QueueOp::Submit(lists) => {
            // Roots recorded before the submission belong before it.
            for &l in &lists {
                rec.flush_root(l);
            }
            let lists: Vec<String> = lists.iter().map(|l| format!("\"{l:#x}\"")).collect();
            format!(
                "{{\"type\":\"submit\",{head},\"lists\":[{}]}}",
                lists.join(",")
            )
        }
        QueueOp::Signal { fence, value } => {
            format!("{{\"type\":\"signal\",{head},\"fence\":\"{fence:#x}\",\"value\":{value}}}")
        }
        QueueOp::Wait { fence, value } => {
            format!("{{\"type\":\"wait\",{head},\"fence\":\"{fence:#x}\",\"value\":{value}}}")
        }
    };
    rec.line(&line);
}

impl Recorder {
    fn now(&self) -> u64 {
        self.began.elapsed().as_micros() as u64
    }

    fn line(&mut self, line: &str) {
        if let Some((_, out)) = &mut self.out {
            // shortcut: a failed write only loses lines of a diagnostic.
            let _ = writeln!(out, "{line}");
        }
    }

    fn flush_root(&mut self, list: usize) {
        if let Some(count) = self.roots.remove(&list) {
            let t = self.now();
            self.line(&format!(
                "{{\"type\":\"cmd\",\"t_us\":{t},\"list\":\"{list:#x}\",\"op\":\"root\",\"count\":{count}}}"
            ));
        }
    }

    fn flush_roots(&mut self) {
        let mut lists: Vec<usize> = self.roots.keys().copied().collect();
        lists.sort_unstable();
        for list in lists {
            self.flush_root(list);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(window: &mut Window, draws: &[u64]) -> Vec<u64> {
        draws
            .iter()
            .enumerate()
            .filter_map(|(i, &d)| window.after(i as u64 + 1, d))
            .collect()
    }

    #[test]
    fn the_window_opens_after_the_first_frame_of_a_thousand_draws() {
        let mut w = Window {
            at: None,
            frames: 3,
            first: None,
        };
        // Frames 1..=8; frame 3 is the first heavy one: 4, 5 and 6 are written.
        assert_eq!(
            frames(&mut w, &[20, 40, 1200, 900, 5000, 5000, 5000, 5000]),
            [4, 5, 6]
        );
        assert!(!w.over(5));
        assert!(w.over(6));
    }

    #[test]
    fn a_frame_number_opens_the_window_whatever_the_draws() {
        let mut w = Window {
            at: Some(3),
            frames: 2,
            first: None,
        };
        assert_eq!(frames(&mut w, &[0, 0, 0, 0, 0, 0]), [3, 4]);
        assert!(w.over(4));
    }

    #[test]
    fn a_window_that_never_opens_is_never_over() {
        let mut w = Window {
            at: None,
            frames: 3,
            first: None,
        };
        assert!(frames(&mut w, &[10, 999, 0]).is_empty());
        assert!(!w.over(3));
    }

    #[test]
    fn commands_are_written_as_json_fields() {
        assert_eq!(
            Cmd::Draw {
                vertices: 3,
                instances: 2
            }
            .fields(),
            "\"draw\",\"vertices\":3,\"instances\":2"
        );
        assert_eq!(
            Cmd::Targets {
                count: 2,
                first: 0x80,
                depth: 0
            }
            .fields(),
            "\"targets\",\"count\":2,\"first\":\"0x80\",\"depth\":\"0x0\""
        );
        assert!(Cmd::Indirect { max: 1 }.is_draw());
        assert!(!Cmd::Dispatch { x: 1, y: 1, z: 1 }.is_draw());
    }
}
