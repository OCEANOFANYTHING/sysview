//! Opt-in durable copy of the dashboard's history window (`serve
//! --history-file`).
//!
//! One compact JSON line per sample, so an outside tool can read the series
//! without talking to the HTTP API:
//!
//! ```json
//! {"t":1730000000,"cpu":12.3,"mem":45.6,"swap":0.1,"rx":1234.5,"tx":678.9}
//! ```
//!
//! Only the five scalar series are recorded. Per-core and per-mount history is
//! not, because a restart cannot honestly reconstruct them — those charts
//! simply start empty again.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// One history line.
#[derive(Serialize, Deserialize)]
struct Sample {
    /// Unix seconds.
    t: u64,
    cpu: f32,
    mem: f32,
    swap: f32,
    rx: f64,
    tx: f64,
}

/// A line from the file, as the ring wants it.
pub type Row = (f32, f32, f32, f64, f64);

/// Appends to (and resumes from) a history file. Counting appended lines is
/// what keeps the trim cheap: the file is only re-read once it has grown past
/// several windows, and never per tick.
pub struct HistoryFile {
    path: PathBuf,
    /// Lines kept when trimming: the ring's own window.
    window: usize,
    /// Lines appended since the last trim.
    lines: usize,
    /// Set after the first write failure; the sink then stays quiet instead of
    /// complaining once per sample.
    dead: bool,
}

impl HistoryFile {
    pub fn new(path: PathBuf, window: usize) -> Self {
        Self {
            path,
            window: window.max(1),
            lines: 0,
            dead: false,
        }
    }

    /// The file's samples, oldest first, capped to one window: what a restart
    /// needs to resume the chart instead of starting blank.
    pub fn replay(&self) -> Vec<Row> {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return Vec::new();
        };
        let mut rows: Vec<Row> = text
            .lines()
            .filter_map(|line| serde_json::from_str::<Sample>(line).ok())
            .map(|s| (s.cpu, s.mem, s.swap, s.rx, s.tx))
            .collect();
        let keep = rows.len().saturating_sub(self.window);
        rows.drain(..keep);
        rows
    }

    /// Appends one sample. The file is trimmed to the newest window whenever it
    /// has grown to four of them, so a long-running server cannot fill a disk.
    pub fn append(&mut self, cpu: f32, mem: f32, swap: f32, rx: f64, tx: f64) {
        if self.dead {
            return;
        }
        let row = Sample {
            t: now_secs(),
            cpu: finite32(cpu),
            mem: finite32(mem),
            swap: finite32(swap),
            rx: finite64(rx),
            tx: finite64(tx),
        };
        let Ok(line) = serde_json::to_string(&row) else {
            return;
        };
        if let Err(e) = append_line(&self.path, &line) {
            self.fault(&e);
            return;
        }
        self.lines += 1;
        if self.lines >= self.window * 3 {
            self.lines = 0;
            if let Err(e) = self.trim() {
                self.fault(&e);
            }
        }
    }

    /// Reports the first write failure and then goes quiet: a read-only or
    /// unwritable path must not produce a line per tick, nor stop sampling.
    fn fault(&mut self, e: &std::io::Error) {
        if !self.dead {
            eprintln!(
                "sysview: history file {}: {e} (history is not being written)",
                self.path.display()
            );
        }
        self.dead = true;
    }

    fn trim(&self) -> std::io::Result<()> {
        let text = std::fs::read_to_string(&self.path)?;
        let mut lines: Vec<&str> = text.lines().collect();
        let keep = lines.len().saturating_sub(self.window);
        lines.drain(..keep);
        write_atomic(&self.path, &format!("{}\n", lines.join("\n")))
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// JSON has no NaN or infinities, and neither is a chart point.
fn finite32(v: f32) -> f32 {
    if v.is_finite() { v } else { 0.0 }
}

fn finite64(v: f64) -> f64 {
    if v.is_finite() { v } else { 0.0 }
}

fn append_line(path: &Path, line: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent()
        && !dir.as_os_str().is_empty()
    {
        std::fs::create_dir_all(dir)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(f, "{line}")
}

/// Replaces the file in one step, so an interrupted trim cannot leave it half
/// written (`rename` overwrites on both Unix and Windows).
fn write_atomic(path: &Path, body: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sysview-history-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    #[test]
    fn appended_samples_replay_in_order() {
        let path = temp("order.jsonl");
        let _ = std::fs::remove_file(&path);
        let mut h = HistoryFile::new(path.clone(), 8);
        for i in 0..5 {
            h.append(i as f32, 0.0, 0.0, 0.0, 0.0);
        }
        let rows = h.replay();
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[0].0, 0.0);
        assert_eq!(rows[4].0, 4.0);
    }

    #[test]
    fn replay_keeps_only_the_newest_window() {
        let path = temp("window.jsonl");
        let _ = std::fs::remove_file(&path);
        let mut h = HistoryFile::new(path.clone(), 4);
        for i in 0..10 {
            h.append(i as f32, 0.0, 0.0, 0.0, 0.0);
        }
        let rows = h.replay();
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].0, 6.0);
    }

    #[test]
    fn the_file_is_trimmed_instead_of_growing_without_bound() {
        let path = temp("trim.jsonl");
        let _ = std::fs::remove_file(&path);
        // 12 samples at a window of 3: the trim fires at 9, leaving 3 lines
        // plus the 3 appended after it.
        let mut h = HistoryFile::new(path.clone(), 3);
        for i in 0..12 {
            h.append(i as f32, 0.0, 0.0, 0.0, 0.0);
        }
        let lines = std::fs::read_to_string(&path).unwrap().lines().count();
        assert!(lines <= 6, "expected a bounded file, found {lines} lines");
        let rows = h.replay();
        assert_eq!(rows[rows.len() - 1].0, 11.0);
    }

    #[test]
    fn a_broken_path_is_reported_once_and_not_retried() {
        // A directory where a file is expected: every write fails.
        let dir = temp("blocked");
        std::fs::create_dir_all(&dir).unwrap();
        let mut h = HistoryFile::new(dir.clone(), 4);
        h.append(1.0, 0.0, 0.0, 0.0, 0.0);
        assert!(h.dead);
        h.append(2.0, 0.0, 0.0, 0.0, 0.0); // must not panic or rewrite
        assert!(h.replay().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn non_finite_values_become_readable_json() {
        let path = temp("nan.jsonl");
        let _ = std::fs::remove_file(&path);
        let mut h = HistoryFile::new(path.clone(), 4);
        h.append(f32::NAN, f32::INFINITY, 0.0, f64::NAN, 1.0);
        assert_eq!(h.replay().len(), 1);
    }
}
