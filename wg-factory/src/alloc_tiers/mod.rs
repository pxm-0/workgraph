//! Allocation: the cheapest tier proven on a kind of unit; each run probes one unproven cheaper
//! tier once per key. Opus is never probed, only escalated to.

use crate::api::{Memory, Size, Stat, Tier};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// Tries before a tier is trusted or ruled out.
pub const MIN_OBS: f64 = 3.0;
/// First-pass rate a tier needs to keep a job.
pub const TARGET: f64 = 0.75;

pub fn stat_key(size: Size) -> String {
    match size {
        Size::Large => String::from("worker:large"),
        Size::Small => String::from("worker:small"),
    }
}

/// `(observations, Laplace pass rate)`.
pub fn tally(s: Option<Stat>) -> (f64, f64) {
    let (pass, fail) = s.map_or((0.0, 0.0), |s| (s.pass, s.fail));
    (pass + fail, (pass + 1.0) / (pass + fail + 2.0))
}

pub fn stat(memory: &Memory, key: &str, tier: Tier) -> Option<Stat> {
    memory
        .models
        .iter()
        .find(|(k, _)| k == key)
        .and_then(|(_, tiers)| tiers.iter().find(|(t, _)| *t == tier).map(|(_, s)| *s))
}

/// The tier for a unit of `key`, and the probes list after this pick.
pub fn pick_tier(memory: &Memory, key: &str, probed: &[String]) -> (Tier, Vec<String>) {
    let st = |t: Tier| tally(stat(memory, key, t));
    let proven = Tier::ALL
        .into_iter()
        .find(|t| st(*t).0 >= MIN_OBS && st(*t).1 >= TARGET);
    let ceiling = proven.unwrap_or(Tier::Opus).index();
    let probe = Tier::ALL[..ceiling]
        .iter()
        .copied()
        .find(|t| st(*t).0 < MIN_OBS && !probed.contains(&format!("{key}:{}", t.name())));
    let mut next = probed.to_vec();
    if let Some(t) = probe {
        next.push(format!("{key}:{}", t.name()));
        return (t, next);
    }
    if let Some(t) = proven {
        return (t, next);
    }
    // Nothing proven: sonnet, unless it already proved too weak.
    let t = if st(Tier::Sonnet).0 >= MIN_OBS {
        Tier::Opus
    } else {
        Tier::Sonnet
    };
    (t, next)
}

/// One tier up per fix round, capped at opus.
pub fn fix_tier(tier: Tier, round: u32) -> Tier {
    let i = (tier.index() + round as usize).min(Tier::ALL.len() - 1);
    Tier::ALL[i]
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn mem(stats: &[(Tier, f64, f64)]) -> Memory {
        Memory {
            models: vec![(
                String::from("worker:small"),
                stats
                    .iter()
                    .map(|(t, p, f)| (*t, Stat { pass: *p, fail: *f }))
                    .collect(),
            )],
            ..Memory::default()
        }
    }

    #[test]
    fn tally_is_laplace() {
        assert_eq!(tally(None), (0.0, 0.5));
        assert_eq!(
            tally(Some(Stat {
                pass: 3.0,
                fail: 1.0
            })),
            (4.0, 4.0 / 6.0)
        );
    }

    #[test]
    fn probes_haiku_then_sonnet_then_defaults_and_never_probes_opus() {
        let m = Memory::default();
        let (t1, p) = pick_tier(&m, "worker:small", &[]);
        let (t2, p) = pick_tier(&m, "worker:small", &p);
        let (t3, p) = pick_tier(&m, "worker:small", &p);
        assert_eq!([t1, t2, t3], [Tier::Haiku, Tier::Sonnet, Tier::Sonnet]);
        assert_eq!(p.len(), 2);
    }

    #[test]
    fn proven_tier_is_used_and_a_weak_sonnet_means_opus() {
        let proven = mem(&[(Tier::Haiku, 1.0, 4.0), (Tier::Sonnet, 19.0, 1.0)]);
        assert_eq!(pick_tier(&proven, "worker:small", &[]).0, Tier::Sonnet);
        let weak = mem(&[(Tier::Haiku, 0.0, 5.0), (Tier::Sonnet, 1.0, 4.0)]);
        assert_eq!(pick_tier(&weak, "worker:small", &[]).0, Tier::Opus);
        // An unproven cheaper tier is probed once below the proven ceiling.
        let probe = mem(&[(Tier::Sonnet, 9.0, 0.0)]);
        assert_eq!(pick_tier(&probe, "worker:small", &[]).0, Tier::Haiku);
    }

    #[test]
    fn fix_tier_climbs_and_caps_at_opus() {
        assert_eq!(fix_tier(Tier::Haiku, 1), Tier::Sonnet);
        assert_eq!(fix_tier(Tier::Haiku, 2), Tier::Opus);
        assert_eq!(fix_tier(Tier::Sonnet, 3), Tier::Opus);
        assert_eq!(stat_key(Size::Large), "worker:large");
    }
}
