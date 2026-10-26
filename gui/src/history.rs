//! Battery percentage history per device, for the 24-hour bars and the drain rate.
//! Kept in `<preset folder>\history-<device>.csv` as `unix_time,percent` lines.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

pub const BARS: usize = 24;
const HOUR: u64 = 3600;
/// Keep two weeks in memory (a battery that reports in 10 % steps can take days per step);
/// the file is trimmed to that once it grows past 64 KB.
const KEEP: u64 = 14 * 24 * HOUR;
const FILE_MAX: u64 = 64 << 10;
/// Unchanged readings are still stored this often, so the bars stay filled.
const REPEAT: u64 = 30 * 60;

pub fn now() -> u64 {
    peripheral_lite::battery::unix_now()
}

pub struct History {
    path: Option<PathBuf>,
    samples: Vec<(u64, u8)>,
}

impl History {
    pub fn load(dir: &Path, sel: &str) -> History {
        let path = dir.join(format!("history-{}.csv", sel.replace(':', "-")));
        let cut = now().saturating_sub(KEEP);
        let samples = fs::read_to_string(&path)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| {
                let (t, p) = l.split_once(',')?;
                Some((t.trim().parse().ok()?, p.trim().parse::<u8>().ok()?.min(100)))
            })
            .filter(|s: &(u64, u8)| s.0 >= cut)
            .collect();
        History {
            path: Some(path),
            samples,
        }
    }

    /// A history that is not stored (demo mode, tests).
    pub fn memory(samples: Vec<(u64, u8)>) -> History {
        History { path: None, samples }
    }

    /// Store a reading when it differs from the last one or the last is older than REPEAT.
    pub fn add(&mut self, t: u64, pct: u8) {
        if self
            .samples
            .last()
            .is_some_and(|&(lt, lp)| lp == pct && t < lt + REPEAT)
        {
            return;
        }
        self.samples.push((t, pct.min(100)));
        let cut = t.saturating_sub(KEEP);
        self.samples.retain(|s| s.0 >= cut);
        if let Some(p) = &self.path {
            let _ = append(p, t, pct, &self.samples);
        }
    }

    /// One value per hour for the last 24 hours (oldest first): the last reading in or before
    /// that hour. None before the first reading.
    pub fn bars(&self, now: u64) -> [Option<u8>; BARS] {
        let mut out = [None; BARS];
        for (i, slot) in out.iter_mut().enumerate() {
            let end = now - (BARS - 1 - i) as u64 * HOUR;
            *slot = self.samples.iter().rev().find(|s| s.0 <= end).map(|s| s.1);
        }
        out
    }

    /// Drain in tenths of a percent per hour over the current discharge, from the drops seen
    /// as they happened (a reading lower than the one before, at most an hour after it): the
    /// last drop and the earliest within 6 h before it, or if those are less than an hour
    /// apart, the latest drop at least an hour before it. Timing drops rather than the first
    /// reading works for batteries that report in 10 % steps (MX Master 3S, MX Keys S): a
    /// reading of 90 % may be anything up to the next step, but the moment it falls to 80 % is
    /// exact. Needs two drops an hour or more apart.
    pub fn drain_tenths(&self) -> Option<u32> {
        let s = &self.samples;
        let mut start = s.len().checked_sub(1)?;
        // walk back while the level does not rise (a rise means it was charging)
        while start > 0 && s[start - 1].1 >= s[start].1 {
            start -= 1;
        }
        let drops: Vec<(u64, u8)> = (start + 1..s.len())
            .filter(|&i| s[i].1 < s[i - 1].1 && s[i].0 <= s[i - 1].0 + HOUR)
            .map(|i| s[i])
            .collect();
        let last = *drops.last()?;
        let first = drops
            .iter()
            .find(|d| d.0 + 6 * HOUR >= last.0)
            .filter(|d| d.0 + HOUR <= last.0)
            .or_else(|| drops.iter().rev().find(|d| d.0 + HOUR <= last.0))?;
        let drop = first.1.saturating_sub(last.1) as u64;
        Some((drop * 10 * HOUR / (last.0 - first.0)) as u32)
    }
}

fn append(path: &Path, t: u64, pct: u8, all: &[(u64, u8)]) -> std::io::Result<()> {
    if fs::metadata(path).is_ok_and(|m| m.len() > FILE_MAX) {
        let text: String = all.iter().map(|(t, p)| format!("{t},{p}\n")).collect();
        return peripheral_lite::config::write_atomic(path, &text).map_err(std::io::Error::other);
    }
    let mut f = fs::OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(f, "{t},{pct}")
}

/// "≈ 41 h left" style text from tenths of an hour.
pub fn hours_text(tenths: u32) -> String {
    if tenths >= 100 {
        format!("≈ {} h left", (tenths + 5) / 10)
    } else {
        format!("≈ {}.{} h left", tenths / 10, tenths % 10)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bars_and_drain() {
        let now = 100 * HOUR;
        // 88 % six hours ago, falling 2 % per hour
        let h = History::memory((0..=6).map(|i| (now - (6 - i) * HOUR, 88 - 2 * i as u8)).collect());
        let b = h.bars(now);
        assert_eq!(b[BARS - 1], Some(76));
        assert_eq!(b[BARS - 7], Some(88));
        assert_eq!(b[0], None);
        assert_eq!(h.drain_tenths(), Some(20));
        // a charge in between: only the part after it counts
        let h = History::memory(vec![
            (now - 5 * HOUR, 50),
            (now - 3 * HOUR, 90),
            (now - 2 * HOUR, 88),
            (now - HOUR, 86),
            (now, 84),
        ]);
        assert_eq!(h.drain_tenths(), Some(20));
        // too short, no drop, or a drop not seen as it happened (the app was not running)
        assert_eq!(History::memory(vec![(now - 600, 50), (now, 49)]).drain_tenths(), None);
        assert_eq!(
            History::memory(vec![(now - 2 * HOUR, 50), (now, 50)]).drain_tenths(),
            None
        );
        assert_eq!(
            History::memory(vec![(now - 9 * HOUR, 60), (now - 8 * HOUR, 59), (now, 50)]).drain_tenths(),
            None
        );
        assert_eq!(History::memory(vec![]).drain_tenths(), None);
        // 10 % steps, a reading every 30 minutes: 90 % until a drop at 10 h, 80 % until 30 h,
        // so 10 % took 20 h; one drop alone says nothing yet
        let level = |i: u64| match i {
            0..20 => 90,
            20..60 => 80,
            _ => 70,
        };
        let steps = |hours: u64| History::memory((0..=hours * 2).map(|i| (i * HOUR / 2, level(i))).collect());
        assert_eq!(steps(29).drain_tenths(), None);
        assert_eq!(steps(31).drain_tenths(), Some(5));
    }

    #[test]
    fn add_skips_repeats_and_texts() {
        let mut h = History::memory(vec![]);
        h.add(1000, 50);
        h.add(1100, 50);
        h.add(1200, 49);
        h.add(1000 + REPEAT + 200, 49);
        assert_eq!(h.samples.len(), 3);
        assert_eq!(hours_text(410), "≈ 41 h left");
        assert_eq!(hours_text(91), "≈ 9.1 h left");
    }
}
