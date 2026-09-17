//! Memory scanner: the cheat-hunting loop of "find every word holding
//! this value, play a little, keep the ones that changed the way you
//! expect". Pulling 2 MiB over the gdb remote protocol once per pass is
//! slow and gdb has no scan-and-diff primitive at all, so this runs
//! inside the worker against its own RAM and hands back only the hits.
//!
//! A scan keeps a snapshot of RAM from its last pass alongside the
//! candidate list, so every filter can compare against what the value
//! was, not only against what the user typed.

/// What a pass keeps. `Exact` is the only one that can start a scan
/// without a snapshot; the others need a previous value to compare to,
/// and a first pass with them keeps everything (`Unknown`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Filter {
    /// Keep every address: a starting point when the value is unknown.
    Unknown,
    Exact(u32),
    Changed,
    Unchanged,
    Increased,
    Decreased,
}

/// A pass the UI asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Request {
    /// 1, 2 or 4 bytes, aligned.
    pub width: u8,
    pub filter: Filter,
    /// Start over rather than narrow the current candidates.
    pub restart: bool,
}

/// What a pass found.
#[derive(Clone, Default)]
pub struct Outcome {
    pub width: u8,
    /// Candidates left after the pass.
    pub count: usize,
    /// The first [`REPORT`] of them with their current values.
    pub hits: Vec<(u32, u32)>,
}

/// Hits reported to the UI; the count says how many more there are.
pub const REPORT: usize = 256;

/// The scan in progress: candidates and the RAM they were last seen in.
pub struct Scan {
    width: u8,
    /// Aligned RAM offsets still in play.
    candidates: Vec<u32>,
    /// RAM as of the last pass, for the comparative filters.
    snapshot: Vec<u8>,
}

/// Scanner value syntax shared by the GUI and the control port:
/// unsigned, decimal or `0x`-prefixed hex.
pub fn parse_value(s: &str) -> Option<u32> {
    match s.strip_prefix("0x") {
        Some(hex) => u32::from_str_radix(hex, 16).ok(),
        None => s.parse::<u32>().ok(),
    }
}

fn read(ram: &[u8], addr: u32, width: u8) -> u32 {
    let a = addr as usize;
    ram[a..a + width as usize]
        .iter()
        .rev()
        .fold(0u32, |v, &b| v << 8 | u32::from(b))
}

impl Scan {
    /// Run one pass over `ram`. `scan` is the state from the previous
    /// pass; `None`, a different width, or `restart` begins a new scan
    /// over the whole of `ram`.
    pub fn pass(scan: Option<Scan>, req: Request, ram: &[u8]) -> (Scan, Outcome) {
        let mut scan = match scan {
            Some(s) if !req.restart && s.width == req.width => s,
            _ => Scan {
                width: req.width,
                candidates: (0..ram.len() as u32).step_by(req.width as usize).collect(),
                snapshot: Vec::new(),
            },
        };
        let w = scan.width;
        // Without a snapshot only Exact can narrow anything; the rest
        // keep every candidate so the next pass has something to compare.
        let has_prev = scan.snapshot.len() == ram.len();
        let snapshot = &scan.snapshot;
        scan.candidates.retain(|&a| {
            let cur = read(ram, a, w);
            match req.filter {
                Filter::Unknown => true,
                Filter::Exact(v) => cur == v & (u32::MAX >> (32 - 8 * u32::from(w))),
                _ if !has_prev => true,
                Filter::Changed => cur != read(snapshot, a, w),
                Filter::Unchanged => cur == read(snapshot, a, w),
                Filter::Increased => cur > read(snapshot, a, w),
                Filter::Decreased => cur < read(snapshot, a, w),
            }
        });
        scan.snapshot.clear();
        scan.snapshot.extend_from_slice(ram);
        let outcome = Outcome {
            width: w,
            count: scan.candidates.len(),
            hits: scan
                .candidates
                .iter()
                .take(REPORT)
                .map(|&a| (a, read(ram, a, w)))
                .collect(),
        };
        (scan, outcome)
    }

    /// Width of the pass that produced the current candidate set.
    pub fn width(&self) -> u8 {
        self.width
    }

    /// Number of candidates left.
    pub fn count(&self) -> usize {
        self.candidates.len()
    }

    /// The first `max` candidates as (RAM offset, value in `ram`, value
    /// when the last pass ran). The two values are equal until something
    /// writes RAM after the pass. Without a snapshot the last-pass value
    /// repeats `ram`.
    pub fn list(&self, ram: &[u8], max: usize) -> Vec<(u32, u32, u32)> {
        let w = self.width;
        let has_prev = self.snapshot.len() == ram.len();
        self.candidates
            .iter()
            .take(max)
            .map(|&a| {
                let cur = read(ram, a, w);
                let prev = if has_prev {
                    read(&self.snapshot, a, w)
                } else {
                    cur
                };
                (a, cur, prev)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(filter: Filter, restart: bool) -> Request {
        Request {
            width: 4,
            filter,
            restart,
        }
    }

    #[test]
    fn exact_then_comparative_passes_narrow_the_candidates() {
        let mut ram = vec![0u8; 64];
        ram[8..12].copy_from_slice(&100u32.to_le_bytes());
        ram[40..44].copy_from_slice(&100u32.to_le_bytes());
        let (scan, r) = Scan::pass(None, req(Filter::Exact(100), true), &ram);
        assert_eq!(r.count, 2);
        assert_eq!(r.hits, [(8, 100), (40, 100)]);
        // One of them drops: only it is kept by "decreased".
        ram[8..12].copy_from_slice(&90u32.to_le_bytes());
        let (scan, r) = Scan::pass(Some(scan), req(Filter::Decreased, false), &ram);
        assert_eq!(r.hits, [(8, 90)]);
        // Unchanged since keeps it; increased drops it.
        let (scan, r) = Scan::pass(Some(scan), req(Filter::Unchanged, false), &ram);
        assert_eq!(r.count, 1);
        let (_, r) = Scan::pass(Some(scan), req(Filter::Increased, false), &ram);
        assert_eq!(r.count, 0);
    }

    #[test]
    fn an_unknown_start_keeps_everything_until_something_moves() {
        let mut ram = vec![0u8; 32];
        let (scan, r) = Scan::pass(None, req(Filter::Unknown, true), &ram);
        assert_eq!(r.count, 8);
        ram[16] = 1;
        let (_, r) = Scan::pass(Some(scan), req(Filter::Changed, false), &ram);
        assert_eq!(r.hits, [(16, 1)]);
    }

    #[test]
    fn a_first_comparative_pass_has_nothing_to_compare_and_keeps_all() {
        let ram = vec![7u8; 16];
        let (_, r) = Scan::pass(None, req(Filter::Changed, true), &ram);
        assert_eq!(r.count, 4);
    }

    #[test]
    fn a_width_change_starts_over() {
        let ram = vec![0x11u8; 16];
        let (scan, _) = Scan::pass(None, req(Filter::Exact(0x1111_1111), true), &ram);
        let narrow = Request {
            width: 1,
            ..req(Filter::Exact(0x11), false)
        };
        let (_, r) = Scan::pass(Some(scan), narrow, &ram);
        assert_eq!(r.count, 16);
    }

    #[test]
    fn parse_value_accepts_decimal_and_hex() {
        assert_eq!(parse_value("100"), Some(100));
        assert_eq!(parse_value("0x64"), Some(100));
        assert_eq!(parse_value("-1"), None);
        assert_eq!(parse_value(""), None);
    }

    #[test]
    fn list_reports_current_and_last_pass_values() {
        let mut ram = vec![0u8; 64];
        ram[8..12].copy_from_slice(&100u32.to_le_bytes());
        ram[40..44].copy_from_slice(&100u32.to_le_bytes());
        let (scan, _) = Scan::pass(None, req(Filter::Exact(100), true), &ram);
        // Mutate one candidate without another pass: the snapshot still
        // holds the value as of the pass, so the columns diverge.
        ram[8..12].copy_from_slice(&90u32.to_le_bytes());
        assert_eq!(scan.list(&ram, 100), vec![(8, 90, 100), (40, 100, 100)]);
        assert_eq!(scan.list(&ram, 1), vec![(8, 90, 100)]);
    }

    #[test]
    fn exact_masks_the_value_to_the_width() {
        let ram = vec![0xFFu8; 8];
        let (_, r) = Scan::pass(
            None,
            Request {
                width: 2,
                ..req(Filter::Exact(0xABCD_FFFF), true)
            },
            &ram,
        );
        assert_eq!(r.count, 4);
    }
}
