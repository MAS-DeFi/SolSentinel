//! Compact in-place progress for long-running `update-full` stages.

use std::{
    io::{self, IsTerminal, Write},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const SPINNER_FRAMES: &[char] = &['|', '/', '-', '\\'];
const SPINNER_INTERVAL: Duration = Duration::from_millis(100);
/// Column where the status text begins, matching the restart detail lines.
const STATUS_COLUMN: usize = 45;

static LIVE_SPINNERS: AtomicUsize = AtomicUsize::new(0);
static PAUSE_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Pauses TTY spinners so a child that inherits stdout cannot be overwritten.
///
/// `systemctl` and `fdctl configure` attach to the terminal. A live `\r` spinner
/// on stdout would clobber their output and a sudo password prompt. The pause
/// ends when this guard is dropped.
#[must_use = "live progress stays paused only while the guard is held"]
pub struct LiveProgressPause {
    _private: (),
}

impl LiveProgressPause {
    /// Stops spinner frames and parks the current line before inherited I/O.
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        let previous = PAUSE_COUNT.fetch_add(1, Ordering::SeqCst);
        if previous == 0 {
            park_spinner_line();
        }
        Self { _private: () }
    }
}

impl Drop for LiveProgressPause {
    fn drop(&mut self) {
        let previous = PAUSE_COUNT.fetch_sub(1, Ordering::SeqCst);
        if previous == 1 {
            park_spinner_line();
        }
    }
}

/// Live status line for one compact `update-full` stage.
pub struct StageProgress {
    enabled: bool,
    tty: bool,
    label: String,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    finished: bool,
}

impl StageProgress {
    /// Starts a stage: spinner and elapsed time on a TTY, otherwise a static line.
    pub fn start(enabled: bool, label: impl Into<String>) -> Self {
        let label = label.into();
        let tty = io::stdout().is_terminal();
        let stop = Arc::new(AtomicBool::new(false));
        let mut worker = None;

        if enabled && tty {
            write_tty_frame(&live_frame(&label, Duration::ZERO, 0));
            let stop_flag = Arc::clone(&stop);
            let thread_label = label.clone();
            let started = Instant::now();
            worker = Some(thread::spawn(move || {
                let mut frame = 1usize;
                while !stop_flag.load(Ordering::SeqCst) {
                    thread::sleep(SPINNER_INTERVAL);
                    if stop_flag.load(Ordering::SeqCst) {
                        break;
                    }
                    if PAUSE_COUNT.load(Ordering::SeqCst) > 0 {
                        continue;
                    }
                    write_tty_frame(&live_frame(&thread_label, started.elapsed(), frame));
                    frame = frame.wrapping_add(1);
                }
            }));
            LIVE_SPINNERS.fetch_add(1, Ordering::SeqCst);
        } else if enabled {
            let _ = writeln!(io::stdout(), "{}", in_progress_line(&label));
            let _ = io::stdout().flush();
        }

        Self {
            enabled,
            tty,
            label,
            stop,
            worker,
            finished: false,
        }
    }

    /// Stops the spinner and writes the final status for this stage.
    pub fn finish(mut self, status: &str) {
        self.stop_worker();
        if self.enabled {
            let line = status_line(&self.label, status);
            if self.tty {
                let mut out = io::stdout().lock();
                let _ = write!(out, "\r{line}\x1b[K\n");
                let _ = out.flush();
            } else {
                let _ = writeln!(io::stdout(), "{line}");
                let _ = io::stdout().flush();
            }
        }
        self.finished = true;
    }

    fn stop_worker(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
            LIVE_SPINNERS.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

impl Drop for StageProgress {
    fn drop(&mut self) {
        self.stop_worker();
        if self.enabled && self.tty && !self.finished {
            let mut out = io::stdout().lock();
            let _ = writeln!(out);
            let _ = out.flush();
        }
    }
}

/// Formats a compact duration as `12s` or `3m 28s`.
pub fn format_duration(duration: Duration) -> String {
    let total_seconds = duration.as_secs();
    let minutes = total_seconds / 60;
    let seconds = total_seconds % 60;
    if minutes == 0 {
        format!("{seconds}s")
    } else {
        format!("{minutes}m {seconds}s")
    }
}

fn park_spinner_line() {
    if LIVE_SPINNERS.load(Ordering::SeqCst) == 0 {
        return;
    }
    let mut out = io::stdout().lock();
    let _ = writeln!(out);
    let _ = out.flush();
}

fn in_progress_line(label: &str) -> String {
    status_line(label, "in progress")
}

fn live_frame(label: &str, elapsed: Duration, frame: usize) -> String {
    let spinner = SPINNER_FRAMES[frame % SPINNER_FRAMES.len()];
    status_line(label, &format!("{} {spinner}", format_duration(elapsed)))
}

fn status_line(label: &str, status: &str) -> String {
    let prefix = label.len().saturating_add(2);
    let dots = STATUS_COLUMN.saturating_sub(prefix).max(3);
    format!("{label} {} {status}", ".".repeat(dots))
}

fn write_tty_frame(line: &str) {
    if PAUSE_COUNT.load(Ordering::SeqCst) > 0 {
        return;
    }
    let mut out = io::stdout().lock();
    let _ = write!(out, "\r{line}\x1b[K");
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::{
        LiveProgressPause, STATUS_COLUMN, format_duration, in_progress_line, live_frame,
        status_line,
    };
    use std::time::Duration;

    #[test]
    fn format_duration_renders_minutes_and_seconds() {
        assert_eq!(format_duration(Duration::from_secs(28)), "28s");
        assert_eq!(format_duration(Duration::from_secs(208)), "3m 28s");
    }

    #[test]
    fn status_line_aligns_status_column() {
        let line = status_line("      stop", "done");
        assert_eq!(&line[STATUS_COLUMN..], "done");
        let configure = status_line("      configure 1/2", "done");
        assert_eq!(&configure[STATUS_COLUMN..], "done");
    }

    #[test]
    fn in_progress_line_marks_the_stage_as_running() {
        let line = in_progress_line("[2/3] Build Firedancer");
        assert!(line.contains("[2/3] Build Firedancer"));
        assert!(line.ends_with("in progress"));
    }

    #[test]
    fn live_frame_includes_elapsed_time_and_spinner() {
        let line = live_frame("[2/3] Build Firedancer", Duration::from_secs(12), 1);
        assert!(line.contains("[2/3] Build Firedancer"));
        assert!(line.ends_with("12s /"));
    }

    #[test]
    fn live_progress_pause_is_reentrant() {
        let outer = LiveProgressPause::new();
        let inner = LiveProgressPause::new();
        drop(inner);
        drop(outer);
    }
}
