//! Balloon policy.
//!
//! Pages a guest has touched stay resident on the host even after the guest frees them, so a
//! VM's RSS only ever grows. virtio-balloon lets the host take free pages back: the target is
//! "how much memory the guest may keep". This module turns guest `/proc/meminfo` numbers into
//! that target; the I/O (query the guest, set the balloon) lives in the caller.
use std::collections::HashMap;

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MemInfo {
    pub total: u64,
    pub free: u64,
    pub available: u64,
    pub cached: u64,
}

/// Parse the guest control channel's `meminfo` reply: whitespace separated
/// `Key: value kB` tokens (`MemTotal: 4009304 kB MemFree: 2440632 kB ...`).
pub fn parse_meminfo(s: &str) -> Option<MemInfo> {
    let mut m: HashMap<&str, u64> = HashMap::new();
    let toks: Vec<&str> = s.split_whitespace().collect();
    for w in toks.windows(2) {
        if let Some(key) = w[0].strip_suffix(':') {
            if let Ok(v) = w[1].parse::<u64>() {
                m.insert(key, v * KIB);
            }
        }
    }
    Some(MemInfo {
        total: *m.get("MemTotal")?,
        free: *m.get("MemFree")?,
        available: *m.get("MemAvailable")?,
        cached: m.get("Cached").copied().unwrap_or(0),
    })
}

#[derive(Debug, Clone, Copy)]
pub struct Policy {
    /// Never shrink the guest below this.
    pub floor: u64,
    /// Memory kept free above what the guest uses, so allocation bursts don't hit reclaim.
    pub headroom: u64,
    /// Ignore changes smaller than this (each balloon move costs guest work).
    pub hysteresis: u64,
    /// Largest single shrink. Taking memory away in one go forces the guest into reclaim (or
    /// balloon deflate-on-OOM) right when it is busiest, so it is walked down instead.
    pub max_shrink_step: u64,
}

impl Default for Policy {
    fn default() -> Self {
        Policy { floor: 1024 * MIB, headroom: 768 * MIB, hysteresis: 128 * MIB, max_shrink_step: 256 * MIB }
    }
}

/// Memory the guest should be allowed to keep, given what it's using now.
///
/// `balloon` is how many bytes the balloon currently holds (configured RAM minus the present
/// target). The guest's `MemTotal` does not shrink when the balloon inflates, so those pages
/// sit outside `MemFree` and would look "used"; they are subtracted, otherwise the target
/// would creep upward on every tick.
///
/// "Used" is everything else that isn't free: anonymous memory *and* page cache. Dropping
/// cache is the caller's decision (it is done when the window is minimised).
pub fn target(mem: &MemInfo, balloon: u64, configured: u64, p: &Policy) -> u64 {
    let used = mem.total.saturating_sub(mem.free).saturating_sub(balloon);
    (used + p.headroom).clamp(p.floor.min(configured), configured)
}

/// Decide the next balloon target. Growing the guest's allowance takes effect at once (it is
/// asking for memory); shrinking is rate-limited and ignores small deltas.
pub fn next_target(current: u64, wanted: u64, p: &Policy) -> Option<u64> {
    if wanted > current {
        return (wanted - current >= p.hysteresis / 2).then_some(wanted);
    }
    let delta = current - wanted;
    (delta >= p.hysteresis).then(|| current - delta.min(p.max_shrink_step))
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1024 * MIB;

    fn mem(total: u64, free: u64) -> MemInfo {
        MemInfo { total, free, available: free, cached: 0 }
    }

    #[test]
    fn parses_agent_reply() {
        let m = parse_meminfo("MemTotal:        4009304 kB MemFree:         2440632 kB MemAvailable:    3038736 kB Cached:           828696 kB").unwrap();
        assert_eq!(m.total, 4009304 * 1024);
        assert_eq!(m.free, 2440632 * 1024);
        assert_eq!(m.available, 3038736 * 1024);
        assert_eq!(m.cached, 828696 * 1024);
    }

    #[test]
    fn rejects_incomplete_or_garbage() {
        assert!(parse_meminfo("MemTotal: 1 kB").is_none());
        assert!(parse_meminfo("error unknown command: meminfo").is_none());
        assert!(parse_meminfo("").is_none());
    }

    #[test]
    fn targets_used_plus_headroom() {
        let p = Policy::default();
        // 4 GiB guest using ~1.6 GiB -> keep ~2.4 GiB
        let m = mem(4 * GIB, 4 * GIB - 1600 * MIB);
        assert_eq!(target(&m, 0, 4 * GIB, &p), 1600 * MIB + 768 * MIB);
    }

    #[test]
    fn balloon_pages_are_not_counted_as_used() {
        let p = Policy::default();
        // Guest really uses 1600 MiB; the balloon holds 2000 MiB, so MemFree only shows
        // 4096 - 1600 - 2000 = 496 MiB. Without the correction the target would be 3.5 GiB.
        let m = mem(4 * GIB, 496 * MIB);
        assert_eq!(target(&m, 2000 * MIB, 4 * GIB, &p), 1600 * MIB + 768 * MIB);
    }

    #[test]
    fn respects_floor_and_ceiling() {
        let p = Policy::default();
        assert_eq!(target(&mem(4 * GIB, 4 * GIB - 100 * MIB), 0, 4 * GIB, &p), 1024 * MIB); // floor
        assert_eq!(target(&mem(4 * GIB, 0), 0, 4 * GIB, &p), 4 * GIB); // ceiling = configured RAM
        // floor can't exceed what the VM was configured with
        assert_eq!(target(&mem(512 * MIB, 0), 0, 512 * MIB, &p), 512 * MIB);
    }

    #[test]
    fn hysteresis_suppresses_small_moves() {
        let p = Policy::default();
        assert_eq!(next_target(2000 * MIB, 1950 * MIB, &p), None); // small shrink ignored
        assert_eq!(next_target(2000 * MIB, 2030 * MIB, &p), None); // tiny growth ignored
        assert_eq!(next_target(2000 * MIB, 2200 * MIB, &p), Some(2200 * MIB));
    }

    #[test]
    fn grows_at_once_but_shrinks_in_steps() {
        let p = Policy::default();
        assert_eq!(next_target(1500 * MIB, 3000 * MIB, &p), Some(3000 * MIB));
        // wants 1100, currently 4096: walk down by max_shrink_step, never jump
        assert_eq!(next_target(4096 * MIB, 1100 * MIB, &p), Some(4096 * MIB - 256 * MIB));
        // within one step it lands exactly on the want
        assert_eq!(next_target(2000 * MIB, 1800 * MIB, &p), Some(1800 * MIB));
    }
}
