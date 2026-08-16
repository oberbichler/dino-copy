use crate::sync::ProgressSink;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

pub struct Progress {
    _multi: MultiProgress,
    bytes: ProgressBar,
    current: ProgressBar,
}

impl Progress {
    /// Creates a progress display for `total_bytes` and `total_files`.
    pub fn new(total_bytes: u64, total_files: u64) -> Self {
        let multi = MultiProgress::new();
        let bytes = multi.add(ProgressBar::new(total_bytes));
        bytes.set_style(
            ProgressStyle::with_template(
                "{spinner} [{elapsed_precise}] [{bar:40}] {bytes}/{total_bytes} ({bytes_per_sec}, ETA {eta})",
            )
            .unwrap()
            .progress_chars("=>-"),
        );
        let current = multi.add(ProgressBar::new(total_files));
        current.set_style(
            ProgressStyle::with_template("  {pos}/{len} files  {wide_msg}").unwrap(),
        );
        Self { _multi: multi, bytes, current }
    }

    pub fn finish(&self) {
        self.bytes.finish();
        self.current.finish_and_clear();
    }
}

impl ProgressSink for Progress {
    fn add_bytes(&self, n: u64) {
        self.bytes.inc(n);
    }
    fn set_current(&self, name: &str) {
        self.current.set_message(name.to_string());
    }
    fn inc_files(&self) {
        self.current.inc(1);
    }
}
