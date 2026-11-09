//! Single-line progress on stderr, redrawn in place. Mirrors Python's
//! `Progress`: only when stderr is a TTY, throttled to 10/s.

use std::io::{IsTerminal, Write};
use std::time::Instant;

pub struct Progress {
    label: String,
    total: usize,
    enabled: bool,
    last: Instant,
    drawn: bool,
}

impl Progress {
    pub fn new(label: impl Into<String>, total: usize, enabled: bool) -> Self {
        Self {
            label: label.into(),
            total: total.max(1),
            enabled: enabled && std::io::stderr().is_terminal(),
            last: Instant::now(),
            drawn: false,
        }
    }

    pub fn update(&mut self, n: usize) {
        if !self.enabled {
            return;
        }
        let now = Instant::now();
        if now.duration_since(self.last).as_secs_f64() < 0.1 && n < self.total {
            return;
        }
        self.last = now;
        let pct = (n * 100 / self.total).min(100);
        let mut err = std::io::stderr();
        let _ = write!(err, "\r  {} {}/{} ({}%)", self.label, n, self.total, pct);
        let _ = err.flush();
        self.drawn = true;
    }

    pub fn done(&mut self) {
        if self.enabled && self.drawn {
            let mut err = std::io::stderr();
            let _ = write!(err, "\r{}\r", " ".repeat(70));
            let _ = err.flush();
            self.drawn = false;
        }
    }
}
