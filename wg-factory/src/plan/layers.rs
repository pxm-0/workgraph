//! Kahn layering: units in one layer never depend on each other, so they run in parallel.

use crate::model::Unit;
use alloc::string::String;
use alloc::vec::Vec;

/// Unit ids by layer, and whether a dependency cycle forced the remaining units one per layer.
pub fn layers_of(units: &[Unit]) -> (Vec<Vec<String>>, bool) {
    let known = |d: &String| units.iter().any(|u| &u.id == d);
    let mut pending: Vec<&Unit> = units.iter().collect();
    let mut placed: Vec<String> = Vec::new();
    let mut layers = Vec::new();
    while !pending.is_empty() {
        let ready: Vec<String> = pending
            .iter()
            .filter(|u| u.depends_on.iter().all(|d| placed.contains(d) || !known(d)))
            .map(|u| u.id.clone())
            .collect();
        if ready.is_empty() {
            layers.extend(pending.iter().map(|u| alloc::vec![u.id.clone()]));
            return (layers, true);
        }
        pending.retain(|u| !ready.contains(&u.id));
        placed.extend(ready.iter().cloned());
        layers.push(ready);
    }
    (layers, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Size, Tier};
    use alloc::string::ToString;
    use alloc::vec;

    fn u(id: &str, deps: &[&str]) -> Unit {
        Unit {
            id: id.to_string(),
            goal: String::new(),
            files: vec![],
            depends_on: deps.iter().map(|d| d.to_string()).collect(),
            criteria: vec![],
            size: Size::Small,
            risky: false,
            risk_reason: None,
            tier: Tier::Sonnet,
            counts_for_tier: true,
        }
    }

    #[test]
    fn independent_units_share_a_layer_and_unknown_dependencies_are_ignored() {
        let (l, cycle) = layers_of(&[u("a", &[]), u("b", &["zz"]), u("c", &["a", "b"])]);
        assert_eq!(l, vec![vec!["a", "b"], vec!["c"]]);
        assert!(!cycle);
    }

    #[test]
    fn a_cycle_runs_the_rest_one_unit_per_layer() {
        let (l, cycle) = layers_of(&[u("a", &[]), u("b", &["c"]), u("c", &["b"])]);
        assert_eq!(l, vec![vec!["a"], vec!["b"], vec!["c"]]);
        assert!(cycle);
    }
}
