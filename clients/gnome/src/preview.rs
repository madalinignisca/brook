//! Inline image previews, GTK half (spec 2026-09-26-image-previews.md §3).
//!
//! Core hands over bytes it sniffed and sized (`preview_file`). They're decoded here only
//! inside glycin's sandbox: bubblewrap, or `flatpak-spawn --sandbox` in a Flatpak. The sandbox
//! is chosen explicitly (never glycin's `Auto`, which runs unsandboxed in a Flatpak development
//! environment) and checked again after loading. One frame only (nothing animates), a 10 s
//! limit, at most two decodes at a time, newest first. There is no other decoder: if glycin
//! can't run, there's no preview.

use std::cell::{Cell, RefCell};
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

use brook_core::{PREVIEW_MAX_PIXELS, PREVIEW_MAX_SIDE};
use glycin::{MemoryFormatSelection, SandboxMechanism, SandboxSelector};

/// Previews fetched by themselves (larger ones wait for "Show preview").
pub const AUTO_MAX_BYTES: u64 = 4 * 1024 * 1024;
/// Longest a decode (load and its one frame) may take.
pub const DECODE_TIMEOUT: Duration = Duration::from_secs(10);
/// Decodes running at once.
pub const IN_FLIGHT: usize = 2;

/// Decoded pixels, RGBA, ready for a `gdk::MemoryTexture`.
pub struct Pixels {
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub rgba: Vec<u8>,
}

/// The sandbox to ask glycin for: never `Auto`, never `NotSandboxed`.
pub fn selector(flatpak: bool) -> SandboxSelector {
    if flatpak {
        SandboxSelector::FlatpakSpawn
    } else {
        SandboxSelector::Bwrap
    }
}

/// What glycin says it ran is a sandbox.
pub fn sandboxed(mechanism: SandboxMechanism) -> bool {
    matches!(
        mechanism,
        SandboxMechanism::Bwrap | SandboxMechanism::FlatpakSpawn
    )
}

fn within_caps(width: u32, height: u32) -> bool {
    width > 0
        && height > 0
        && width <= PREVIEW_MAX_SIDE
        && height <= PREVIEW_MAX_SIDE
        && width as u64 * height as u64 <= PREVIEW_MAX_PIXELS
}

/// Whether a frame's claims hold for its buffer: RGBA, rows at least `width * 4` bytes, and
/// enough bytes for every row (the last one needn't be padded). All arithmetic is checked.
/// Anything else would reach `gdk::MemoryTexture::new`, which refuses it and makes gtk-rs
/// panic: one crafted image would crash the app.
pub fn frame_fits(rgba: bool, width: u32, height: u32, stride: u32, len: usize) -> bool {
    if !rgba || width == 0 || height == 0 {
        return false;
    }
    let (width, height, stride) = (width as usize, height as usize, stride as usize);
    let Some(row) = width.checked_mul(4) else {
        return false;
    };
    if stride < row {
        return false;
    }
    stride
        .checked_mul(height - 1)
        .and_then(|n| n.checked_add(row))
        .is_some_and(|need| len >= need)
}

pub fn in_flatpak() -> bool {
    Path::new("/.flatpak-info").exists()
}

/// Decode the first frame of `bytes` in the sandbox, within the caps and the time limit.
/// `None`: no preview (glycin missing or refused, not sandboxed, over the caps, too slow).
/// Runs on the Tokio runtime.
pub async fn decode(bytes: Vec<u8>) -> Option<Pixels> {
    // Cancelled however this ends (the timeout, or the caller dropping it): glycin then stops
    // the sandboxed decoder, so a stuck one doesn't keep burning CPU.
    let cancellable = glycin_gio::Cancellable::new();
    let _cancel = CancelOnDrop(cancellable.clone());
    let work = async {
        let mut loader = glycin::Loader::new_vec(bytes);
        loader
            .sandbox_selector(selector(in_flatpak()))
            .accepted_memory_formats(MemoryFormatSelection::R8g8b8a8)
            .cancellable(cancellable.clone())
            .pool(own_pool());
        let image = match loader.load().await {
            Ok(image) => image,
            Err(err) => {
                log_once(&format!("image previews are off: {err}"));
                return None;
            }
        };
        if !sandboxed(image.active_sandbox_mechanism()) {
            log_once("image previews are off: the decoder isn't sandboxed");
            return None;
        }
        // The decoder's own reading of the size, against the same caps as core's header.
        let details = image.details();
        if !within_caps(details.width(), details.height()) {
            return None;
        }
        let frame = image.next_frame().await.ok()?; // one frame: nothing animates
        if !within_caps(frame.width(), frame.height()) {
            return None;
        }
        // The frame's size, stride, format and buffer come from the sandboxed process, which a
        // hostile image may have taken over: check they describe the buffer, or no preview.
        let rgba = frame.memory_format() == glycin::MemoryFormat::R8g8b8a8;
        let buf = frame.buf_slice();
        if !frame_fits(
            rgba,
            frame.width(),
            frame.height(),
            frame.stride(),
            buf.len(),
        ) {
            return None;
        }
        Some(Pixels {
            width: frame.width(),
            height: frame.height(),
            stride: frame.stride() as usize,
            rgba: buf.to_vec(),
        })
    };
    // Dropping the loader on timeout ends the sandboxed process with it.
    tokio::time::timeout(DECODE_TIMEOUT, work)
        .await
        .ok()
        .flatten()
}

/// A pool of its own for each decode. glycin's global pool keeps a decoder process for 30 s
/// and gives it the next images too, so a hostile image that took over one process would see
/// the images decoded after it. With a pool per image, no other image ever goes to that
/// process, and it runs one operation at a time.
fn own_pool() -> std::sync::Arc<glycin::Pool> {
    let mut config = glycin::PoolConfig::new();
    config.max_parallel_operations(1);
    glycin::Pool::new(config)
}

/// Cancels on drop.
struct CancelOnDrop(glycin_gio::Cancellable);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        use glycin_gio::prelude::CancellableExt;
        self.0.cancel();
    }
}

fn log_once(why: &str) {
    thread_local! { static SAID: Cell<bool> = const { Cell::new(false) }; }
    if !SAID.with(|s| s.replace(true)) {
        tracing::info!("{why}");
    }
}

/// Called once when a job has finished, whatever the outcome.
pub type Done = Box<dyn FnOnce()>;

/// A preview waiting to run: `alive` says whether its row is still shown; `run` starts it
/// and calls `done` when it has finished, whatever the outcome.
pub struct Job {
    pub alive: Box<dyn Fn() -> bool>,
    pub run: Box<dyn FnOnce(Done)>,
}

/// At most `max` previews at a time, the newest request first (the rows just shown), and a
/// request whose row has gone is dropped before it starts. On the GTK loop.
pub struct Queue {
    max: usize,
    running: Cell<usize>,
    waiting: RefCell<Vec<Job>>,
}

impl Queue {
    pub fn new(max: usize) -> Rc<Self> {
        Rc::new(Self {
            max,
            running: Cell::new(0),
            waiting: RefCell::new(Vec::new()),
        })
    }

    pub fn push(self: &Rc<Self>, job: Job) {
        self.waiting.borrow_mut().push(job);
        self.pump();
    }

    fn pump(self: &Rc<Self>) {
        while self.running.get() < self.max {
            let next = self.waiting.borrow_mut().pop(); // newest first
            let Some(job) = next else { return };
            if !(job.alive)() {
                continue;
            }
            self.running.set(self.running.get() + 1);
            let me = Rc::downgrade(self);
            (job.run)(Box::new(move || {
                if let Some(me) = me.upgrade() {
                    me.running.set(me.running.get() - 1);
                    me.pump();
                }
            }));
        }
    }
}

thread_local! {
    /// The app's one preview queue.
    pub static QUEUE: Rc<Queue> = Queue::new(IN_FLIGHT);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sandbox_is_chosen_never_inferred() {
        assert!(matches!(selector(false), SandboxSelector::Bwrap));
        assert!(matches!(selector(true), SandboxSelector::FlatpakSpawn));
        assert!(sandboxed(SandboxMechanism::Bwrap));
        assert!(sandboxed(SandboxMechanism::FlatpakSpawn));
        assert!(!sandboxed(SandboxMechanism::NotSandboxed));
    }

    #[test]
    fn the_queue_runs_two_newest_first_and_skips_gone_rows() {
        let q = Queue::new(2);
        let log: Rc<RefCell<Vec<&'static str>>> = Rc::default();
        let finish: Rc<RefCell<Vec<Done>>> = Rc::default();
        let job = |name: &'static str, alive: bool| Job {
            alive: Box::new(move || alive),
            run: Box::new({
                let (log, finish) = (log.clone(), finish.clone());
                move |done| {
                    log.borrow_mut().push(name);
                    finish.borrow_mut().push(done);
                }
            }),
        };
        q.push(job("a", true));
        q.push(job("b", true));
        q.push(job("c", true)); // waits: two are running
        q.push(job("gone", false));
        q.push(job("d", true));
        assert_eq!(*log.borrow(), ["a", "b"]);
        let done = finish.borrow_mut().remove(0);
        done(); // a slot frees: the newest waiting that's still shown
        assert_eq!(*log.borrow(), ["a", "b", "d"]);
        let done = finish.borrow_mut().remove(0);
        done(); // "gone" is skipped, "c" runs
        assert_eq!(*log.borrow(), ["a", "b", "d", "c"]);
    }

    /// However a decode ends (the timeout, or its row going away), glycin's decoder is told
    /// to stop.
    #[test]
    fn ending_a_decode_cancels_the_sandboxed_decoder() {
        use glycin_gio::prelude::CancellableExt;
        let c = glycin_gio::Cancellable::new();
        {
            let _guard = CancelOnDrop(c.clone());
            assert!(!c.is_cancelled());
        }
        assert!(c.is_cancelled());
    }

    /// Every decode gets its own pool, never glycin's shared one.
    #[test]
    fn each_decode_gets_its_own_pool() {
        let (a, b) = (own_pool(), own_pool());
        assert!(!std::sync::Arc::ptr_eq(&a, &b));
        assert!(!std::sync::Arc::ptr_eq(&a, &glycin::Pool::global()));
    }

    #[test]
    fn a_frame_that_lies_about_its_buffer_is_dropped() {
        // 10 x 4 RGBA, stride 40: 3 full rows plus a last row of 40 bytes = 160.
        assert!(frame_fits(true, 10, 4, 40, 160));
        assert!(
            frame_fits(true, 10, 4, 48, 3 * 48 + 40),
            "padded rows, unpadded last row"
        );
        assert!(!frame_fits(true, 10, 4, 40, 159), "one byte short");
        assert!(
            !frame_fits(true, 10, 4, 39, 1000),
            "a stride shorter than a row"
        );
        assert!(!frame_fits(false, 10, 4, 40, 160), "not RGBA");
        assert!(!frame_fits(true, 0, 4, 40, 160));
        assert!(!frame_fits(true, 10, 0, 40, 160));
        // Sizes whose byte counts overflow are refused, never wrapped.
        assert!(!frame_fits(true, u32::MAX, u32::MAX, u32::MAX, usize::MAX));
        assert!(
            !frame_fits(true, 8192, 8192, 32768, 16),
            "a short buffer claiming a big frame"
        );
    }

    #[test]
    fn the_decoders_reading_is_capped_too() {
        assert!(within_caps(8192, 4096));
        assert!(!within_caps(8193, 1));
        assert!(!within_caps(8000, 8000));
        assert!(!within_caps(0, 5));
    }

    /// A real decode through glycin's sandbox (needs glycin's loaders and bubblewrap).
    #[test]
    #[ignore]
    fn a_png_decodes_in_the_sandbox() {
        // A 2×1 PNG, red then blue.
        let png: &[u8] = &[
            0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00,
            0x00, 0x7b, 0x40, 0xe8, 0xdd, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9c, 0x63, 0xf8, 0xcf, 0x00, 0x04, 0xff, 0x01, 0x07, 0x00, 0x01, 0xff, 0xe2, 0x23,
            0x9e, 0x59, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
        ];
        let rt = tokio::runtime::Runtime::new().unwrap();
        let px = rt
            .block_on(decode(png.to_vec()))
            .expect("decoded in the sandbox");
        assert_eq!((px.width, px.height), (2, 1));
        assert_eq!(&px.rgba[..4], &[255, 0, 0, 255]);
    }
}
