//! `sysview top`: a full-screen process monitor.
//!
//! Deliberately dependency-free: the only terminal work is a `stty` call on
//! Unix and `_kbhit`/`_getch` on Windows. When stdin is not a terminal (a
//! pipe, a CI runner) the screen still refreshes; there are simply no keys.

use std::io::Write;
use std::time::{Duration, Instant};

use crate::collect::{TopFrame, TopState};
use crate::render::{self, RenderOpts};
use crate::{Cli, SortKey};

/// Below this a refresh has no useful CPU delta to compute.
const MIN_INTERVAL: f64 = 0.25;

pub fn run(cli: &Cli) -> std::io::Result<()> {
    let opts = RenderOpts::from_cli(cli);
    let mut state = TopState::new();
    let mut sort = cli.sort;
    let mut reverse = cli.reverse;
    let mut rows = cli.top.max(1);
    let interval = cli.interval.max(MIN_INTERVAL);

    let mut term = Term::new()?;
    let mut out = anstream::stdout();

    loop {
        let frame = state.frame(sort, reverse, rows);
        let _ = write!(out, "\x1b[2J\x1b[H{}", draw(&frame, &opts, sort, reverse));
        let _ = out.flush();

        match term.wait(interval) {
            Key::None => {}
            Key::Quit => break,
            Key::Sort(next) => sort = next,
            Key::Reverse => reverse = !reverse,
            Key::Rows(delta) => rows = (rows as isize + delta).clamp(1, 200) as usize,
        }
    }

    // Leave the screen and the cursor as the shell expects them.
    let _ = write!(out, "\x1b[2J\x1b[H");
    let _ = out.flush();
    Ok(())
}

fn draw(f: &TopFrame, opts: &RenderOpts, sort: SortKey, reverse: bool) -> String {
    let direction = if reverse { "asc" } else { "desc" };
    let mut s = format!(
        "sysview top  {}   up {}   {:.1}% CPU ({} logical cores)   {:.1}% memory\n",
        if f.hostname.is_empty() {
            "localhost"
        } else {
            &f.hostname
        },
        render::uptime(f.uptime_secs),
        f.cpu_usage,
        f.logical_cores,
        f.memory.used_percent,
    );
    s.push_str(&render::processes(&f.processes, opts));
    s.push_str(&format!(
        "\nsort {} ({direction}), {} rows   \
         q quit  c cpu  m mem  p pid  n name  r reverse  +/- rows\n",
        sort_name(sort),
        f.processes.len()
    ));
    s
}

fn sort_name(sort: SortKey) -> &'static str {
    match sort {
        SortKey::Cpu => "cpu",
        SortKey::Mem => "memory",
        SortKey::Pid => "pid",
        SortKey::Name => "name",
        SortKey::Uptime => "uptime",
        SortKey::Starttime => "start time",
    }
}

enum Key {
    None,
    Quit,
    Sort(SortKey),
    Reverse,
    Rows(isize),
}

fn map_key(b: u8) -> Key {
    match b {
        // 0x03 is Ctrl+C, 0x1b is Esc: both quit before the shell sees them.
        b'q' | b'Q' | 0x03 | 0x1b => Key::Quit,
        b'c' | b'C' => Key::Sort(SortKey::Cpu),
        b'm' | b'M' => Key::Sort(SortKey::Mem),
        b'p' | b'P' => Key::Sort(SortKey::Pid),
        b'n' | b'N' => Key::Sort(SortKey::Name),
        b'r' | b'R' => Key::Reverse,
        b'+' | b'=' => Key::Rows(5),
        b'-' | b'_' => Key::Rows(-5),
        _ => Key::None,
    }
}

/// Blocks up to `secs`, returning the first key pressed.
fn wait_for_key(interval: f64, mut poll: impl FnMut() -> Option<u8>) -> Key {
    let deadline = Instant::now() + Duration::from_secs_f64(interval);
    loop {
        if let Some(b) = poll() {
            return map_key(b);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Key::None;
        }
        std::thread::sleep(left.min(Duration::from_millis(20)));
    }
}

// ---------------------------------------------------------------- unix -----

#[cfg(unix)]
struct Term {
    /// Set only when stdin is a terminal we put into non-canonical mode.
    raw: bool,
}

#[cfg(unix)]
impl Term {
    fn new() -> std::io::Result<Self> {
        use std::io::IsTerminal;
        if !std::io::stdin().is_terminal() {
            return Ok(Self { raw: false });
        }
        // Not `raw`: the line discipline stays on so "\n" works. -isig keeps
        // Ctrl+C from killing us mid-frame and leaving echo off.
        let raw = std::process::Command::new("stty")
            .args(["-icanon", "-echo", "-isig", "min", "0", "time", "0"])
            .stdin(std::process::Stdio::inherit())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        Ok(Self { raw })
    }

    fn poll(&mut self) -> Option<u8> {
        if !self.raw {
            return None;
        }
        use std::io::Read;
        let mut b = [0u8; 1];
        matches!(std::io::stdin().read(&mut b), Ok(1)).then(|| b[0])
    }

    fn wait(&mut self, secs: f64) -> Key {
        wait_for_key(secs, || self.poll())
    }
}

#[cfg(unix)]
impl Drop for Term {
    fn drop(&mut self) {
        if self.raw {
            let _ = std::process::Command::new("stty").arg("sane").status();
        }
    }
}

// ------------------------------------------------------------- windows -----

#[cfg(windows)]
struct Term;

#[cfg(windows)]
unsafe extern "C" {
    fn _kbhit() -> i32;
    fn _getch() -> i32;
}

#[cfg(windows)]
impl Term {
    fn new() -> std::io::Result<Self> {
        Ok(Self)
    }

    /// `_kbhit` first: the loop must never block waiting for a key, or the
    /// refresh stops.
    fn poll(&mut self) -> Option<u8> {
        // SAFETY: neither call takes a pointer or has preconditions, and
        // `_kbhit` is the documented guard for `_getch`.
        unsafe { (_kbhit() != 0).then(|| _getch() as u8) }
    }

    fn wait(&mut self, secs: f64) -> Key {
        wait_for_key(secs, || self.poll())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quit_is_q_esc_or_ctrl_c() {
        for b in [b'q', b'Q', 0x1b, 0x03] {
            assert!(matches!(map_key(b), Key::Quit), "{b:#x} should quit");
        }
    }

    #[test]
    fn sort_keys_cover_the_common_columns() {
        assert!(matches!(map_key(b'c'), Key::Sort(SortKey::Cpu)));
        assert!(matches!(map_key(b'm'), Key::Sort(SortKey::Mem)));
        assert!(matches!(map_key(b'p'), Key::Sort(SortKey::Pid)));
        assert!(matches!(map_key(b'n'), Key::Sort(SortKey::Name)));
        assert!(matches!(map_key(b'r'), Key::Reverse));
        assert!(matches!(map_key(b'+'), Key::Rows(5)));
        assert!(matches!(map_key(b'-'), Key::Rows(-5)));
        assert!(matches!(map_key(b'z'), Key::None));
    }

    #[test]
    fn a_key_press_is_returned_before_the_interval_ends() {
        let mut presses = vec![b'm'].into_iter();
        let key = wait_for_key(5.0, || presses.next());
        assert!(matches!(key, Key::Sort(SortKey::Mem)));
    }
}
