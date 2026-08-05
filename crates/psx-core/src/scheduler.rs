//! Event-driven scheduler.
//!
//! Components register events at absolute cycle deadlines instead of being
//! ticked every CPU cycle; the system runs the CPU until the earliest
//! deadline, then fires due events. Accuracy comes from scheduling events at
//! the exact cycle they occur on hardware.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub enum EventKind {
    VBlank,
    TimerTarget(u8),
    DmaComplete(u8),
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

    pub fn schedule(&mut self, deadline: u64, kind: EventKind) {
        self.heap.push(Reverse(Entry {
            deadline,
            seq: self.seq,
            kind,
        }));
        self.seq += 1;
    }

    /// Cycle of the earliest pending event, if any.
    pub fn next_deadline(&self) -> Option<u64> {
        self.heap.peek().map(|Reverse(e)| e.deadline)
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
        s.schedule(10, EventKind::TimerTarget(1));
        assert_eq!(s.next_deadline(), Some(10));
        assert_eq!(s.pop_due(15), Some(EventKind::TimerTarget(1)));
        assert_eq!(s.pop_due(15), None); // vblank not due yet
        assert_eq!(s.pop_due(25), Some(EventKind::VBlank));
    }
}
