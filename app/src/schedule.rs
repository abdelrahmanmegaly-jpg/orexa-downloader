// Scheduled downloads: queued downloads wait for a set time (at night, say), then start a
// few at a time as usual. An optional stop time puts the ones still running back in the
// queue, to continue at the next start time. Orexa has to be running (the tray is enough).

use super::*;
use chrono::Timelike;
use std::sync::Mutex;

/// What a queued row says while it waits ("Starts at 02:00"), shared with the row drawing.
static QUEUE_NOTE: Mutex<String> = Mutex::new(String::new());

pub fn queue_note() -> String {
    let n = QUEUE_NOTE.lock().unwrap().clone();
    if n.is_empty() {
        "Waiting for a free slot".into()
    } else {
        n
    }
}

/// "2:05", "02:05" or "14:30" as minutes after midnight.
pub fn parse_hm(s: &str) -> Option<u32> {
    let (h, m) = s.trim().split_once(':')?;
    let (h, m) = (h.trim().parse::<u32>().ok()?, m.trim().parse::<u32>().ok()?);
    (h < 24 && m < 60 && s.trim().len() <= 5).then_some(h * 60 + m)
}

/// Did the clock pass `t` going from minute `last` to minute `now` (across midnight too)?
pub fn crossed(last: u32, now: u32, t: u32) -> bool {
    if last == now {
        false
    } else if last < now {
        last < t && t <= now
    } else {
        t > last || t <= now
    }
}

/// Is minute `m` inside the window from `start` up to `stop` (which may be past midnight)?
pub fn in_window(m: u32, start: u32, stop: u32) -> bool {
    if start <= stop {
        start <= m && m < stop
    } else {
        m >= start || m < stop
    }
}

impl App {
    /// Open or close the scheduled window as the clock passes the start and stop times.
    pub fn tick_schedule(&mut self) {
        let start = parse_hm(&self.sched_start);
        let stop = parse_hm(&self.sched_stop);
        if !self.sched_on || start.is_none() {
            self.sched_active = false;
            self.sched_last_min = None;
            *QUEUE_NOTE.lock().unwrap() = String::new();
            return;
        }
        let now = chrono::Local::now();
        let m = now.hour() * 60 + now.minute();
        match self.sched_last_min.replace(m) {
            // Just switched on, or the app just started: inside a set window counts as started.
            None => {
                self.sched_active = match (start, stop) {
                    (Some(a), Some(b)) => in_window(m, a, b),
                    _ => false,
                };
            }
            Some(last) => {
                if let Some(a) = start {
                    if crossed(last, m, a) && !self.sched_active {
                        self.sched_active = true;
                        if self.notify_done && self.downloads.iter().any(|d| d.status == Status::Queued) {
                            shell::toast("Scheduled downloads started".into(), String::new());
                        }
                    }
                }
                if let Some(b) = stop {
                    if crossed(last, m, b) && self.sched_active {
                        self.sched_active = false;
                        // Back in the queue, with what they saved, for next time.
                        let running: Vec<u64> =
                            self.downloads.iter().filter(|d| d.status == Status::Downloading).map(|d| d.id).collect();
                        for id in running {
                            self.engine.cancel(id);
                        }
                    }
                }
            }
        }
        // Without a stop time, the window closes once the queue is done.
        if stop.is_none()
            && self.sched_active
            && !self.downloads.iter().any(|d| matches!(d.status, Status::Downloading | Status::Queued))
        {
            self.sched_active = false;
        }
        *QUEUE_NOTE.lock().unwrap() = if self.sched_active { String::new() } else { format!("Starts at {}", self.sched_start.trim()) };
    }

    /// May the queue start downloads by itself right now?
    pub fn queue_open(&self) -> bool {
        !self.sched_on || parse_hm(&self.sched_start).is_none() || self.sched_active
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_are_read() {
        assert_eq!(parse_hm("02:00"), Some(120));
        assert_eq!(parse_hm("2:05"), Some(125));
        assert_eq!(parse_hm(" 23:59 "), Some(1439));
        assert_eq!(parse_hm("24:00"), None);
        assert_eq!(parse_hm("7"), None);
        assert_eq!(parse_hm("aa:bb"), None);
    }

    #[test]
    fn the_start_is_noticed_even_across_midnight() {
        assert!(crossed(119, 120, 120), "the minute it happens");
        assert!(!crossed(120, 120, 120), "only once");
        assert!(crossed(110, 125, 120), "a minute skipped while busy");
        assert!(crossed(1439, 0, 0), "midnight");
        assert!(crossed(1430, 5, 2), "across midnight");
        assert!(!crossed(1430, 5, 600));
    }

    #[test]
    fn windows_can_run_past_midnight() {
        assert!(in_window(120, 60, 420));
        assert!(!in_window(500, 60, 420));
        assert!(in_window(30, 1380, 420), "23:00 to 07:00 includes 00:30");
        assert!(!in_window(720, 1380, 420));
    }
}
