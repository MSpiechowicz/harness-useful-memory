//! Compact plain-text replies: size caps, dates and ages.

use std::time::{SystemTime, UNIX_EPOCH};

/// Room kept for the `… (n more)` trailer.
const TRAILER_ROOM: usize = 24;

/// Blocks of text joined by newlines, never longer than a byte budget. Blocks that do not fit
/// are counted and reported as `… (n more)`.
pub struct Capped {
    text: String,
    max: usize,
    omitted: usize,
}

impl Capped {
    /// An empty reply of at most `max` bytes.
    pub fn new(max: usize) -> Self {
        Self {
            text: String::new(),
            max,
            omitted: 0,
        }
    }

    /// Adds `block` whole, or counts it as omitted once anything was omitted or it does not fit.
    /// Says whether the block was added.
    pub fn push(&mut self, block: &str) -> bool {
        if self.omitted > 0 || !self.fits(block.len()) {
            self.omitted += 1;
            return false;
        }

        self.append(block);

        true
    }

    /// Adds `block` whole when it fits, else as much of it as fits (cut at a character boundary,
    /// ending with `…`). Once a block was cut or omitted, later blocks are counted as omitted.
    pub fn push_cut(&mut self, block: &str) {
        if self.omitted > 0 {
            self.omitted += 1;
            return;
        }

        if self.fits(block.len()) {
            self.append(block);
            return;
        }

        let separator = usize::from(!self.text.is_empty());
        let room = self.max.saturating_sub(self.text.len() + separator + TRAILER_ROOM);

        if room < 16 {
            self.omitted += 1;
            return;
        }

        let mut end = room - '…'.len_utf8();
        while !block.is_char_boundary(end) {
            end -= 1;
        }

        self.append(&format!("{}…", &block[..end]));
    }

    /// The reply text.
    pub fn finish(mut self) -> String {
        if self.omitted > 0 {
            if !self.text.is_empty() {
                self.text.push('\n');
            }

            self.text.push_str(&format!("… ({} more)", self.omitted));
        }

        self.text
    }

    fn fits(&self, length: usize) -> bool {
        let separator = usize::from(!self.text.is_empty());
        self.text.len() + separator + length + TRAILER_ROOM <= self.max
    }

    fn append(&mut self, block: &str) {
        if !self.text.is_empty() {
            self.text.push('\n');
        }

        self.text.push_str(block);
    }
}

/// The current time in Unix seconds.
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// `YYYY-MM-DD` (UTC) of Unix seconds.
pub fn date(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);

    let (year, month, day) = crate::support::time::civil_from_days(days);

    format!("{year:04}-{month:02}-{day:02}")
}

/// `today`, `yesterday`, `N days ago`, `N months ago` or `N years ago`.
pub fn age(seconds: i64, now: i64) -> String {
    let days = (now - seconds).max(0) / 86_400;

    match days {
        0 => "today".to_owned(),
        1 => "yesterday".to_owned(),
        2..=59 => format!("{days} days ago"),
        60..=729 => format!("{} months ago", days / 30),
        _ => format!("{} years ago", days / 365),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_are_utc_calendar_days() {
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(951_782_400), "2000-02-29");
        assert_eq!(date(1_791_158_400), "2026-10-05");
    }

    #[test]
    fn capped_reports_what_it_left_out() {
        let mut reply = Capped::new(60);
        reply.push("first block");
        reply.push(&"x".repeat(80));
        reply.push("small");
        assert_eq!(reply.finish(), "first block\n… (2 more)");
    }

    #[test]
    fn capped_push_says_whether_the_block_was_added() {
        let mut reply = Capped::new(60);
        assert!(reply.push("first block"));
        assert!(!reply.push(&"x".repeat(80)));
        assert!(!reply.push("small"));
    }
}
