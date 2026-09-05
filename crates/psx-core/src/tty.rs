//! Kernel TTY capture (`putchar` through the A0h/B0h jump tables).
//!
//! The buffer is host-side output, not machine state: it is never
//! serialized, survives state loads and resets alongside the other
//! ambient assets, and is bounded so a chatty title cannot grow it without
//! limit. Consumers keep a *monotonic* cursor (bytes ever written) rather
//! than an index into the retained text, so trimming the front of the
//! buffer and rebuilding the machine both leave their read position valid.

/// Retained-text cap. Once exceeded, the oldest half is dropped at a line
/// boundary so the retained text still starts on a whole line.
const CAP: usize = 1024 * 1024;

#[derive(Default, Clone)]
pub struct Tty {
    text: String,
    /// Bytes trimmed from the front so far; `dropped + text.len()` is the
    /// monotonic end position.
    dropped: u64,
    /// Byte index (into `text`) where the current, unterminated line
    /// starts; lets the newline hook log a line without rescanning.
    line_start: usize,
}

impl Tty {
    pub fn push(&mut self, ch: char) -> Option<&str> {
        self.text.push(ch);
        if ch != '\n' {
            if self.text.len() > CAP {
                self.trim();
            }
            return None;
        }
        let line = &self.text[self.line_start..self.text.len() - 1];
        self.line_start = self.text.len();
        Some(line)
    }

    fn trim(&mut self) {
        // The halfway point may fall inside a multi-byte char; the text is
        // longer than CAP here, so a boundary at or after it always exists
        let half = (CAP / 2..)
            .find(|&i| self.text.is_char_boundary(i))
            .unwrap();
        let cut = self.text[..half].rfind('\n').map_or(half, |i| i + 1);
        self.text.drain(..cut);
        self.dropped += cut as u64;
        self.line_start = self.line_start.saturating_sub(cut);
    }

    /// All retained text.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Monotonic position just past the last byte written.
    pub fn end(&self) -> u64 {
        self.dropped + self.text.len() as u64
    }

    /// Text written after position `pos`, and the position to pass next
    /// time. Bytes already trimmed are gone; the slice then starts at the
    /// oldest retained byte.
    pub fn since(&self, pos: u64) -> (&str, u64) {
        let start = pos.saturating_sub(self.dropped).min(self.text.len() as u64) as usize;
        (&self.text[start..], self.end())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push_str(t: &mut Tty, s: &str) {
        for c in s.chars() {
            t.push(c);
        }
    }

    #[test]
    fn newline_yields_the_completed_line() {
        let mut t = Tty::default();
        assert_eq!(t.push('a'), None);
        assert_eq!(t.push('b'), None);
        assert_eq!(t.push('\n'), Some("ab"));
        assert_eq!(t.push('c'), None);
        assert_eq!(t.push('\n'), Some("c"));
    }

    #[test]
    fn cursor_is_incremental() {
        let mut t = Tty::default();
        push_str(&mut t, "one\n");
        let (s, pos) = t.since(0);
        assert_eq!(s, "one\n");
        push_str(&mut t, "two\n");
        let (s, pos) = t.since(pos);
        assert_eq!(s, "two\n");
        assert_eq!(t.since(pos).0, "");
    }

    #[test]
    fn trimming_keeps_the_cursor_monotonic() {
        let mut t = Tty::default();
        let line = "x".repeat(99) + "\n";
        let mut pos = 0;
        let mut total = 0usize;
        for _ in 0..(CAP / 100 + 500) {
            push_str(&mut t, &line);
            total += line.len();
            let (s, next) = t.since(pos);
            assert!(s.is_empty() || s.starts_with('x'));
            pos = next;
        }
        assert_eq!(pos, total as u64);
        assert!(t.text().len() <= CAP);
        assert!(t.text().starts_with('x'));
        assert_eq!(t.push('\n'), Some(""));
    }
}
