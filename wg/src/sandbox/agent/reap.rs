//! `sandbox-agent reap --older-than <secs>` (design §8.5 step 8): removes only objects labelled
//! `wg.job` whose `wg.created` is older than the cut-off. There is never an unfiltered prune, so
//! anything else on the machine, argus's included, is never touched.

use super::podman::Podman;
use serde_json::{json, Value};
use std::time::Duration;

const T: Duration = Duration::from_secs(120);

/// `(kind, list argv, id field, remove argv prefix)`. Containers go first, with `--volumes`:
/// `pod rm` would take them but leave the anonymous volumes an image's `VOLUME` made.
fn kinds() -> [(&'static str, Vec<String>, &'static str, Vec<String>); 3] {
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    [
        (
            "containers",
            s(&["ps", "-a", "--filter", "label=wg.job", "--format", "json"]),
            "Id",
            s(&["rm", "-f", "--volumes"]),
        ),
        (
            "pods",
            s(&["pod", "ps", "--filter", "label=wg.job", "--format", "json"]),
            "Id",
            s(&["pod", "rm", "-f"]),
        ),
        (
            "volumes",
            s(&[
                "volume",
                "ls",
                "--filter",
                "label=wg.job",
                "--format",
                "json",
            ]),
            "Name",
            s(&["volume", "rm", "-f"]),
        ),
    ]
}

/// Ids among `listed` that carry `wg.job` and are older than `older` seconds at `now`. An object
/// with `wg.job` but no readable `wg.created` is ours and stale.
pub fn stale(listed: &Value, id_field: &str, older: u64, now: u64) -> Vec<String> {
    listed
        .as_array()
        .into_iter()
        .flatten()
        .filter(|o| o["Labels"]["wg.job"].is_string())
        // An infra container can only go with its pod.
        .filter(|o| o["IsInfra"] != true)
        .filter(|o| {
            o["Labels"]["wg.created"]
                .as_str()
                .and_then(|c| c.parse::<u64>().ok())
                .is_none_or(|c| now.saturating_sub(c) > older)
        })
        .filter_map(|o| o[id_field].as_str().map(str::to_string))
        .collect()
}

pub fn reap(pm: &dyn Podman, older: u64, now: u64) -> Result<Value, String> {
    let mut removed = serde_json::Map::new();
    let mut failed = Vec::new();
    for (kind, list, id, rm) in kinds() {
        let r = pm.run(&list, None, T);
        if !r.ok() {
            return Err(format!(
                "podman cannot list {kind}: {}",
                crate::ids::one_line(&r.out, 200)
            ));
        }
        let listed: Value = serde_json::from_str(r.out.trim()).unwrap_or(json!([]));
        let ids = stale(&listed, id, older, now);
        for i in &ids {
            let mut a = rm.clone();
            a.push(i.clone());
            if !pm.run(&a, None, T).ok() {
                failed.push(format!("{kind} {i}"));
            }
        }
        removed.insert(kind.into(), json!(ids.len()));
    }
    Ok(json!({ "removed": removed, "failed": failed }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::agent::run::tests::{ran, Fake};
    use std::sync::Mutex;

    #[test]
    fn only_old_labelled_objects_go() {
        let listed = json!([
            { "Id": "old", "Labels": { "wg.job": "j-1", "wg.created": "100" } },
            { "Id": "new", "Labels": { "wg.job": "j-2", "wg.created": "9950" } },
            { "Id": "nocreated", "Labels": { "wg.job": "j-3" } },
            { "Id": "decoy", "Labels": { "app": "argus" } },
            { "Id": "nolabels" },
            { "Id": "infra", "IsInfra": true, "Labels": { "wg.job": "j-1", "wg.created": "1" } }
        ]);
        assert_eq!(stale(&listed, "Id", 7200, 10_000), ["old", "nocreated"]);
    }

    #[test]
    fn reap_lists_by_label_and_removes_each() {
        let pm = Fake {
            calls: Mutex::new(vec![]),
            rules: vec![
                (
                    "pod ps",
                    ran(
                        0,
                        r#"[{"Id":"p1","Labels":{"wg.job":"j-1","wg.created":"1"}}]"#,
                    ),
                ),
                (
                    "volume ls",
                    ran(
                        0,
                        r#"[{"Name":"v1","Labels":{"wg.job":"j-1","wg.created":"1"}}]"#,
                    ),
                ),
                ("volume rm -f v1", ran(1, "busy")),
                (
                    "ps -a",
                    ran(
                        0,
                        r#"[{"Id":"c1","Labels":{"wg.job":"j-1","wg.created":"1"}}]"#,
                    ),
                ),
            ],
        };
        let out = reap(&pm, 7200, 100_000).unwrap();
        assert_eq!(
            out["removed"],
            json!({ "pods": 1, "containers": 1, "volumes": 1 })
        );
        assert_eq!(out["failed"], json!(["volumes v1"]));
        let calls = pm.calls.lock().unwrap().clone();
        let at = |c: &str| calls.iter().position(|x| x == c).unwrap();
        assert!(
            at("rm -f --volumes c1") < at("pod rm -f p1"),
            "containers and their volumes go before the pod"
        );
        assert!(calls.iter().all(|c| !c.contains("prune")));
        assert!(calls
            .iter()
            .filter(|c| c.contains("--filter"))
            .all(|c| c.contains("label=wg.job")));
        let broken = Fake {
            calls: Mutex::new(vec![]),
            rules: vec![("pod ps", ran(125, "no podman"))],
        };
        assert!(reap(&broken, 1, 1)
            .unwrap_err()
            .contains("cannot list pods"));
    }
}
