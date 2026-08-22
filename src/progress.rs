use colored::Colorize;
use indicatif::{ProgressBar, ProgressStyle};
use std::borrow::Cow;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone)]
pub struct IndexProgressBar {
    bar: ProgressBar,
    total_bytes: u64,
    total_files: usize,
    files_indexed: Arc<AtomicUsize>,
    bytes_indexed: Arc<AtomicU64>,
    symbols_indexed: Arc<AtomicUsize>,
    enabled: bool,
}

impl IndexProgressBar {
    /// Create a new ANSI indexing progress bar calibrated to the total bytes of code.
    pub fn new(total_bytes: u64, total_files: usize, enabled: bool) -> Self {
        if !enabled {
            return Self {
                bar: ProgressBar::hidden(),
                total_bytes,
                total_files,
                files_indexed: Arc::new(AtomicUsize::new(0)),
                bytes_indexed: Arc::new(AtomicU64::new(0)),
                symbols_indexed: Arc::new(AtomicUsize::new(0)),
                enabled: false,
            };
        }

        // Avoid 0 division in indicatif if total_bytes is 0
        let bar_len = total_bytes.max(1);
        let bar = ProgressBar::new(bar_len);

        let style = ProgressStyle::default_bar()
            .template("{spinner:.cyan.bold} [{elapsed_precise}] [{bar:30.cyan/blue}] {bytes}/{total_bytes} ({percent}%) {msg}")
            .unwrap_or_else(|_| ProgressStyle::default_bar())
            .progress_chars("━╸─")
            .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏");

        bar.set_style(style);
        bar.enable_steady_tick(Duration::from_millis(80));

        Self {
            bar,
            total_bytes,
            total_files,
            files_indexed: Arc::new(AtomicUsize::new(0)),
            bytes_indexed: Arc::new(AtomicU64::new(0)),
            symbols_indexed: Arc::new(AtomicUsize::new(0)),
            enabled: true,
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Record a completed file, incrementing progress by the file size in bytes.
    pub fn inc_file(&self, rel_path: &str, file_bytes: u64, symbols_count: usize) {
        self.bytes_indexed.fetch_add(file_bytes, Ordering::Relaxed);
        let current_files = self.files_indexed.fetch_add(1, Ordering::Relaxed) + 1;
        let total_symbols = self
            .symbols_indexed
            .fetch_add(symbols_count, Ordering::Relaxed)
            + symbols_count;

        if self.enabled {
            self.bar.inc(file_bytes);
            let char_count = rel_path.chars().count();
            let display_name = if char_count > 36 {
                let skip = char_count.saturating_sub(33);
                let suffix: String = rel_path.chars().skip(skip).collect();
                format!("...{}", suffix)
            } else {
                rel_path.to_string()
            };
            self.bar.set_message(format!(
                "{} ({}/{} files, {} symbols)",
                display_name.cyan(),
                current_files,
                self.total_files,
                total_symbols
            ));
        }
    }

    /// Update status message for subsequent phases (trait resolution, calls, imports, CozoDB sync).
    pub fn set_phase(&self, phase_name: &str) {
        if self.enabled {
            self.bar.set_message(format!("{}", phase_name.yellow()));
        }
    }

    /// Finish progress bar and clear from terminal.
    pub fn finish_and_clear(&self) {
        if self.enabled {
            self.bar.finish_and_clear();
        }
    }

    /// Finish progress bar with a final status message.
    pub fn finish_with_message(&self, msg: impl Into<Cow<'static, str>>) {
        if self.enabled {
            let finish_style = ProgressStyle::default_bar()
                .template("{spinner:.green.bold} [{elapsed_precise}] [{bar:30.green}] {bytes}/{total_bytes} (100%) {msg}")
                .unwrap_or_else(|_| ProgressStyle::default_bar())
                .progress_chars("━━─")
                .tick_chars("✓✓✓✓✓✓✓✓✓✓");
            self.bar.set_style(finish_style);
            self.bar.set_position(self.total_bytes);
            self.bar.finish_with_message(msg);
        }
    }

    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    pub fn total_files(&self) -> usize {
        self.total_files
    }

    pub fn files_indexed(&self) -> usize {
        self.files_indexed.load(Ordering::Relaxed)
    }

    pub fn bytes_indexed(&self) -> u64 {
        self.bytes_indexed.load(Ordering::Relaxed)
    }

    pub fn symbols_indexed(&self) -> usize {
        self.symbols_indexed.load(Ordering::Relaxed)
    }
}

/// Format a byte count into a human-friendly string (e.g. "452.1 KB", "12.4 MB").
pub fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_bytes() {
        assert_eq!(format_bytes(500), "500 B");
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(1024 * 1024), "1.00 MB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.00 MB");
        assert_eq!(format_bytes(2 * 1024 * 1024 * 1024), "2.00 GB");
    }

    #[test]
    fn test_hidden_progress_bar() {
        let pb = IndexProgressBar::new(1000, 10, false);
        assert_eq!(pb.total_bytes(), 1000);
        assert_eq!(pb.total_files(), 10);
        pb.inc_file("src/main.rs", 250, 5);
        assert_eq!(pb.bytes_indexed(), 250);
        assert_eq!(pb.files_indexed(), 1);
        assert_eq!(pb.symbols_indexed(), 5);
        pb.set_phase("Testing phase");
        pb.finish_with_message("Done");
    }

    #[test]
    fn test_enabled_progress_bar() {
        let pb = IndexProgressBar::new(2000, 2, true);
        pb.inc_file("src/lib.rs", 1000, 10);
        pb.inc_file("src/main.rs", 1000, 12);
        assert_eq!(pb.bytes_indexed(), 2000);
        assert_eq!(pb.files_indexed(), 2);
        assert_eq!(pb.symbols_indexed(), 22);
        pb.finish_and_clear();
    }
}
