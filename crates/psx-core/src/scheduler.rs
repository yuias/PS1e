//! Event-driven scheduler.
//!
//! A heap of absolute cycle deadlines. `step()` pops every entry due at or
//! before the current cycle count after each instruction, so an event fires
//! at the end of the first instruction whose cycle count reaches its
//! deadline. Components re-arm themselves from their own state after being
//! serviced and after register writes that can move their deadline; entries
//! are never cancelled, so a superseded entry is a harmless early wake-up,
//! and a re-arm at a past cycle simply means "again next instruction".
//!
//! VBlank is the only event scheduled so far.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

/// What a due event wakes up.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub enum EventKind {
    VBlank,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
struct Entry {
    deadline: u64,
    seq: u64, // tie-breaker keeping FIFO order for same-cycle events
    kind: EventKind,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Scheduler {
    heap: BinaryHeap<Reverse<Entry>>,
    seq: u64,
}

impl Scheduler {
    pub fn new() -> Self {
        Self {
            heap: BinaryHeap::new(),
            seq: 0,
        }
    }

    /// Push unconditionally (VBlank: exactly one entry is ever pending).
    pub fn schedule(&mut self, deadline: u64, kind: EventKind) {
        self.heap.push(Reverse(Entry {
            deadline,
            seq: self.seq,
            kind,
        }));
        self.seq += 1;
    }

    /// Make sure `kind` fires no later than `deadline`: push unless an entry
    /// of the same kind is already pending at or before it. Components whose
    /// handler recomputes their own deadline use this from register writes
    /// without growing the heap on every access.
    pub fn wake_by(&mut self, deadline: u64, kind: EventKind) {
        let already_covered = self
            .heap
            .iter()
            .any(|Reverse(e)| e.kind == kind && e.deadline <= deadline);
        if !already_covered {
            self.schedule(deadline, kind);
        }
    }

    /// Pop the next event if it is due at or before `now`.
    pub fn pop_due(&mut self, now: u64) -> Option<EventKind> {
        match self.heap.peek() {
            Some(Reverse(e)) if e.deadline <= now => {
                let Reverse(e) = self.heap.pop().unwrap();
                Some(e.kind)
            }
            _ => None,
        }
    }
}

impl Default for Scheduler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fires_in_deadline_order() {
        let mut s = Scheduler::new();
        s.schedule(20, EventKind::VBlank);
        s.schedule(10, EventKind::VBlank);
        assert_eq!(s.pop_due(5), None); // nothing due yet
        assert_eq!(s.pop_due(15), Some(EventKind::VBlank)); // the 10 entry
        assert_eq!(s.pop_due(15), None); // the 20 entry not due yet
        assert_eq!(s.pop_due(25), Some(EventKind::VBlank));
        assert_eq!(s.pop_due(25), None);
    }

    #[test]
    fn wake_by_skips_when_an_earlier_entry_is_pending() {
        let mut s = Scheduler::new();
        s.wake_by(10, EventKind::VBlank);
        s.wake_by(20, EventKind::VBlank);
        assert_eq!(s.pop_due(30), Some(EventKind::VBlank));
        assert_eq!(s.pop_due(30), None);
    }

    #[test]
    fn wake_by_adds_an_earlier_entry() {
        let mut s = Scheduler::new();
        s.wake_by(20, EventKind::VBlank);
        s.wake_by(10, EventKind::VBlank);
        assert_eq!(s.pop_due(15), Some(EventKind::VBlank));
        assert_eq!(s.pop_due(25), Some(EventKind::VBlank));
        assert_eq!(s.pop_due(25), None);
    }
}
