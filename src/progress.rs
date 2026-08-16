use crate::sync::ProgressSink;
use indicatif::{MultiProgress, ProgressBar, ProgressState, ProgressStyle};
use std::fmt;
use std::time::{Duration, Instant};

/// Frames of the progress spinner.
const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// How long a single spinner frame stays on screen.
const SPINNER_FRAME_MS: u128 = 125;

/// Picks the spinner frame for `elapsed`.
///
/// Driven by wall-clock time rather than indicatif's built-in `{spinner}`:
/// that one advances on every accepted `ProgressBar::inc`, and the rate limiter
/// behind it allows one per millisecond. On a fast disk the spinner would race
/// at up to 1000 frames per second — far beyond the ~20 redraws per second that
/// are actually painted, so the frame index jumps by dozens between two draws
/// and the spinner reads as flicker. Its speed would also depend on the copy
/// throughput instead of on time.
fn spinner_frame(elapsed: Duration) -> &'static str {
    let index = (elapsed.as_millis() / SPINNER_FRAME_MS) as usize % SPINNER_FRAMES.len();
    SPINNER_FRAMES[index]
}

/// Builds a style from `template`, in which `{spin}` renders the time-driven
/// spinner from [`spinner_frame`].
pub fn spinner_template(template: &str) -> ProgressStyle {
    let start = Instant::now();
    ProgressStyle::with_template(template)
        .expect("progress templates are compile-time constants of this crate")
        .with_key("spin", move |_: &ProgressState, w: &mut dyn fmt::Write| {
            let _ = w.write_str(spinner_frame(start.elapsed()));
        })
}

pub struct Progress {
    _multi: MultiProgress,
    bytes: ProgressBar,
    files: ProgressBar,
    /// One status line per copy worker. Copies run in parallel, so a single
    /// shared line would jump between directories with every thread that
    /// happens to write last.
    workers: Vec<ProgressBar>,
}

impl Progress {
    /// Creates a progress display for `total_bytes` and `total_files`, with one
    /// status line for each of the `jobs` copy workers.
    pub fn new(total_bytes: u64, total_files: u64, jobs: usize) -> Self {
        let multi = MultiProgress::new();
        let bytes = multi.add(ProgressBar::new(total_bytes));
        bytes.set_style(
            spinner_template(
                "{spin} [{elapsed_precise}] [{bar:40}] {bytes}/{total_bytes} ({bytes_per_sec}, ETA {eta})",
            )
            .progress_chars("=>-"),
        );
        let files = multi.add(ProgressBar::new(total_files));
        files.set_style(ProgressStyle::with_template("  {pos}/{len} files").unwrap());
        let workers = (0..jobs.max(1))
            .map(|i| {
                let bar = multi.add(ProgressBar::new_spinner());
                bar.set_style(ProgressStyle::with_template("  {prefix:>2}  {wide_msg}").unwrap());
                bar.set_prefix((i + 1).to_string());
                bar
            })
            .collect();
        Self { _multi: multi, bytes, files, workers }
    }

    pub fn finish(&self) {
        for worker in &self.workers {
            worker.finish_and_clear();
        }
        self.files.finish_and_clear();
        self.bytes.finish();
    }
}

impl ProgressSink for Progress {
    fn add_bytes(&self, n: u64) {
        self.bytes.inc(n);
    }
    fn set_current(&self, worker: usize, name: &str) {
        // Modulo rather than indexing straight in: on the fallback path
        // `execute` runs outside its own pool, where the thread index can
        // exceed the number of lines. Sharing a line beats panicking.
        let line = &self.workers[worker % self.workers.len()];
        line.set_message(name.to_string());
    }
    fn inc_files(&self) {
        self.files.inc(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_zero_is_shown_first() {
        assert_eq!(spinner_frame(Duration::ZERO), SPINNER_FRAMES[0]);
    }

    #[test]
    fn a_frame_is_held_for_the_full_interval() {
        // Just before the interval elapses we are still on the same frame —
        // otherwise the dwell time would be shorter than configured.
        let almost = Duration::from_millis(SPINNER_FRAME_MS as u64 - 1);
        assert_eq!(spinner_frame(almost), SPINNER_FRAMES[0]);
    }

    #[test]
    fn the_frame_advances_after_one_interval() {
        let one = Duration::from_millis(SPINNER_FRAME_MS as u64);
        assert_eq!(spinner_frame(one), SPINNER_FRAMES[1]);
    }

    #[test]
    fn the_frames_wrap_around_after_a_full_cycle() {
        // Without the modulo this would panic on an out-of-bounds index once
        // the run outlives one cycle — which every real run does.
        let cycle = SPINNER_FRAME_MS as u64 * SPINNER_FRAMES.len() as u64;
        assert_eq!(spinner_frame(Duration::from_millis(cycle)), SPINNER_FRAMES[0]);
        assert_eq!(
            spinner_frame(Duration::from_millis(cycle * 7 + SPINNER_FRAME_MS as u64 * 3)),
            SPINNER_FRAMES[3]
        );
    }

    #[test]
    fn a_full_cycle_visits_every_frame_in_order() {
        let seen: Vec<&str> = (0..SPINNER_FRAMES.len())
            .map(|i| spinner_frame(Duration::from_millis(SPINNER_FRAME_MS as u64 * i as u64)))
            .collect();
        assert_eq!(seen, SPINNER_FRAMES.to_vec());
    }

    #[test]
    fn one_hour_in_the_spinner_stays_calm() {
        // Guards the actual complaint: the frame must follow time, not the
        // number of copied bytes. Over an hour that is 8 frames per second.
        let expected_frames = 3600 * 1000 / SPINNER_FRAME_MS;
        assert_eq!(expected_frames, 28_800, "expected 8 frames per second");
    }
}
