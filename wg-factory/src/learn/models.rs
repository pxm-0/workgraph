//! Model stats: did the allocated tier get its unit through checks without a fix?

use super::cap_pair;
use crate::api::{Stat, Tier};
use crate::model::ModelOutcome;
use alloc::string::String;
use alloc::vec::Vec;

pub type Models = Vec<(String, Vec<(Tier, Stat)>)>;

pub fn update(models: &Models, outcomes: &[ModelOutcome]) -> Models {
    let mut m = models.clone();
    for o in outcomes {
        let pos = match m.iter().position(|(k, _)| *k == o.key) {
            Some(p) => p,
            None => {
                m.push((o.key.clone(), Vec::new()));
                m.len() - 1
            }
        };
        let tiers = &mut m[pos].1;
        let cur = tiers.iter().find(|(t, _)| *t == o.tier).map_or(
            Stat {
                pass: 0.0,
                fail: 0.0,
            },
            |(_, s)| *s,
        );
        let (pass, fail) = cap_pair(
            cur.pass + if o.pass { 1.0 } else { 0.0 },
            cur.fail + if o.pass { 0.0 } else { 1.0 },
        );
        let next = Stat { pass, fail };
        match tiers.iter().position(|(t, _)| *t == o.tier) {
            Some(i) => tiers[i] = (o.tier, next),
            None => tiers.push((o.tier, next)),
        }
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn o(key: &str, tier: Tier, pass: bool) -> ModelOutcome {
        ModelOutcome {
            key: String::from(key),
            tier,
            pass,
        }
    }

    #[test]
    fn counts_for_and_against_and_caps_at_twenty() {
        let start: Models = vec![(
            String::from("worker:small"),
            vec![(
                Tier::Sonnet,
                Stat {
                    pass: 19.0,
                    fail: 1.0,
                },
            )],
        )];
        let got = update(
            &start,
            &[
                o("worker:small", Tier::Sonnet, true),
                o("worker:large", Tier::Haiku, false),
            ],
        );
        assert_eq!(
            got[0].1,
            vec![(
                Tier::Sonnet,
                Stat {
                    pass: 19.05,
                    fail: 0.95
                }
            )]
        );
        assert_eq!(
            got[1],
            (
                String::from("worker:large"),
                vec![(
                    Tier::Haiku,
                    Stat {
                        pass: 0.0,
                        fail: 1.0
                    }
                )]
            )
        );
    }
}
