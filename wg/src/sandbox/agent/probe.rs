//! The container-isolation probe (design §8.4 check 5): a fixed script, compiled in, run inside a
//! step-profile and a service-profile container built by the job's own argv builders, then judged
//! here from its output and from `podman inspect`. Any doubt is a failure.

use crate::sandbox::job::Limits;
use serde_json::Value;

/// Runs inside the probe image (busybox). It reads and lists; it never runs a Mac command.
pub const SCRIPT: &str = r#"echo "uid=$(id -u)"; echo "gid=$(id -g)"
for p in /Users /mnt/mac /opt/orbstack-guest /etc/argus /srv/argus; do [ -e "$p" ] && echo "host_path=$p"; done
for d in $(echo "$PATH" | tr ':' ' ') /usr /bin /sbin /opt; do [ -d "$d" ] && find "$d" \( -name mac -o -name macctl -o -name orb -o -name orbctl \) 2>/dev/null; done | while read -r f; do [ -x "$f" ] && echo "mac_cmd=$f"; done
echo "net=$(ls /sys/class/net | tr '\n' ' ')"
command -v nc >/dev/null && echo "nc=present"
nc -w 2 1.1.1.1 443 </dev/null >/dev/null 2>&1 && echo "tcp=connected"
grep -E '^(CapEff|CapPrm|CapBnd|NoNewPrivs):' /proc/self/status | tr -d ' \t' | sed 's/:/=/'
touch /.wg-probe 2>/dev/null && echo "root=writable"
echo "memory_max=$(cat /sys/fs/cgroup/memory.max)"; echo "pids_max=$(cat /sys/fs/cgroup/pids.max)"
echo "cpu_max=$(tr ' ' '/' < /sys/fs/cgroup/cpu.max)"
echo "uid_map=$(head -n 1 /proc/self/uid_map | tr -s ' ' | sed 's/^ //' | tr ' ' '/')"
echo "---mountinfo"; cat /proc/self/mountinfo"#;

/// The service profile's capability set: CHOWN(0) DAC_OVERRIDE(1) FOWNER(3) SETGID(6) SETUID(7).
pub const SERVICE_CAP_MASK: &str = "00000000000000cb";
const NO_CAPS: &str = "0000000000000000";
const HOST_PATHS: [&str; 5] = [
    "/Users",
    "/mnt/mac",
    "/opt/orbstack-guest",
    "/etc/argus",
    "/srv/argus",
];
const NETWORK_FS: [&str; 6] = ["virtiofs", "9p", "nfs", "nfs4", "cifs", "smb3"];
const DEVICES: [&str; 6] = ["/null", "/zero", "/full", "/random", "/urandom", "/tty"];

#[derive(Debug, Default)]
pub struct Probe {
    pub vars: Vec<(String, String)>,
    pub mounts: Vec<String>,
}

impl Probe {
    pub fn parse(out: &str) -> Probe {
        let (head, mounts) = out.split_once("---mountinfo").unwrap_or((out, ""));
        Probe {
            vars: head
                .lines()
                .filter_map(|l| l.split_once('='))
                .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
                .collect(),
            mounts: mounts
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(str::to_string)
                .collect(),
        }
    }

    pub fn get(&self, k: &str) -> Option<&str> {
        self.vars
            .iter()
            .find(|(x, _)| x == k)
            .map(|(_, v)| v.as_str())
    }

    fn all(&self, k: &str) -> Vec<&str> {
        self.vars
            .iter()
            .filter(|(x, _)| x == k)
            .map(|(_, v)| v.as_str())
            .collect()
    }
}

/// Which container a probe ran in.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Profile {
    Step,
    Service,
}

/// One mountinfo line is acceptable: no network or host filesystem, and any bind is the job's own
/// volume, a file podman generates for the container, a standard device or a proc mask.
fn mount_ok(line: &str, job: &str) -> Result<(), String> {
    let (pre, post) = line.split_once(" - ").ok_or("unreadable mountinfo")?;
    let f: Vec<&str> = pre.split_whitespace().collect();
    let (root, at) = (
        *f.get(3).ok_or("short mountinfo")?,
        *f.get(4).ok_or("short mountinfo")?,
    );
    let fstype = post.split_whitespace().next().unwrap_or("");
    if NETWORK_FS.contains(&fstype) || fstype.starts_with("fuse") {
        return Err(format!("{at} is a {fstype} mount"));
    }
    if HOST_PATHS
        .iter()
        .any(|h| at == *h || at.starts_with(&format!("{h}/")))
    {
        return Err(format!("{at} is mounted"));
    }
    let own_volume = root.contains(&format!("/volumes/wgsbx-{job}-")) && root.ends_with("/_data");
    let generated = root.contains("/overlay-containers/") && root.contains("/userdata/");
    let device = fstype == "devtmpfs" && DEVICES.contains(&root);
    if root == "/" || own_volume || generated || device || fstype == "proc" {
        Ok(())
    } else {
        Err(format!("{at} is a bind of host path {root}"))
    }
}

/// Every inside check for one profile; the first failure names its check.
pub fn judge(p: &Probe, profile: Profile, job: &str, l: &Limits) -> Result<(), String> {
    let fail = |check: &str, why: String| Err(format!("container_isolation_failed:{check}: {why}"));
    if p.get("uid").is_none() || p.mounts.is_empty() {
        return fail("probe", "the probe printed no answers".into());
    }
    for m in &p.mounts {
        if let Err(e) = mount_ok(m, job) {
            return fail("mounts", e);
        }
    }
    if let Some(h) = p.all("host_path").first() {
        return fail("host_paths", format!("{h} exists inside"));
    }
    if let Some(c) = p.all("mac_cmd").first() {
        return fail("mac_commands", format!("{c} is executable inside"));
    }
    if p.get("net") != Some("lo") {
        return fail(
            "network",
            format!("interfaces {:?}", p.get("net").unwrap_or("")),
        );
    }
    if p.get("nc") != Some("present") || p.get("tcp").is_some() {
        return fail(
            "network",
            "a TCP connect was possible or could not be tried".into(),
        );
    }
    let map_host = p.get("uid_map").and_then(|m| m.split('/').nth(1));
    if map_host.is_none_or(|h| h == "0") {
        return fail("user", format!("container root maps to {:?}", map_host));
    }
    if profile == Profile::Step && (p.get("uid"), p.get("gid")) != (Some("1000"), Some("1000")) {
        return fail("user", "the step does not run as 1000:1000".into());
    }
    let caps = if profile == Profile::Step {
        NO_CAPS
    } else {
        SERVICE_CAP_MASK
    };
    for k in ["CapEff", "CapPrm", "CapBnd"] {
        if p.get(k) != Some(caps) {
            return fail("capabilities", format!("{k} is {:?}, not {caps}", p.get(k)));
        }
    }
    if p.get("NoNewPrivs") != Some("1") {
        return fail("capabilities", "NoNewPrivs is not set".into());
    }
    if p.get("root") == Some("writable") {
        return fail("root", "the root filesystem is writable".into());
    }
    let tmp_size = format!("size={}k", u64::from(l.tmp_mb) * 1024);
    let tmp_ok = p.mounts.iter().any(|m| {
        let at = m.split_whitespace().nth(4);
        at == Some("/tmp") && m.contains(" - tmpfs ") && m.contains(&tmp_size)
    });
    if !tmp_ok {
        return fail("root", "/tmp is not the sized tmpfs".into());
    }
    let want = [
        (
            "memory_max",
            (u64::from(l.memory_mb) * 1024 * 1024).to_string(),
        ),
        ("pids_max", l.pids.to_string()),
        ("cpu_max", format!("{}/100000", u64::from(l.cpus) * 100_000)),
    ];
    for (k, v) in want {
        if p.get(k) != Some(v.as_str()) {
            return fail("limits", format!("{k} is {:?}, not {v}", p.get(k)));
        }
    }
    Ok(())
}

/// The outside view (`podman inspect`): only the job's named volumes, never a bind.
pub fn judge_inspect(container: &Value, job: &str) -> Result<(), String> {
    let c = if container.is_array() {
        &container[0]
    } else {
        container
    };
    let mounts = c["Mounts"]
        .as_array()
        .ok_or("container_isolation_failed:mounts: inspect has no Mounts")?;
    for m in mounts {
        let name = m["Name"].as_str().unwrap_or("");
        if m["Type"] != "volume" || !name.starts_with(&format!("wgsbx-{job}-")) {
            return Err(format!(
                "container_isolation_failed:mounts: inspect shows a {} mount of {}",
                m["Type"].as_str().unwrap_or("?"),
                m["Source"].as_str().unwrap_or("?")
            ));
        }
    }
    Ok(())
}

/// The pod's infra container must have network mode `none`.
pub fn judge_network(infra: &Value) -> Result<(), String> {
    let c = if infra.is_array() { &infra[0] } else { infra };
    match c["HostConfig"]["NetworkMode"].as_str() {
        Some("none") => Ok(()),
        other => Err(format!(
            "container_isolation_failed:network: the pod's network mode is {other:?}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::job::CEILING;

    const JOB: &str = "j-00000000000000aa";

    fn mounts() -> String {
        [
            "1 0 0:1 / / ro,relatime - overlay overlay rw,lowerdir=/x",
            "2 1 0:2 / /tmp rw,nosuid,nodev - tmpfs tmpfs rw,size=524288k,uid=1100",
            "3 1 0:3 / /proc rw - proc proc rw",
            "4 1 0:38 /scon/c/rootfs/home/wgsbx/.local/share/containers/storage/volumes/wgsbx-j-00000000000000aa-src/_data /work rw - btrfs /dev/vdb1 rw",
            "5 1 0:72 /containers/overlay-containers/abc/userdata/hosts /etc/hosts ro - tmpfs tmpfs rw",
            "6 1 0:7 /null /dev/null rw - devtmpfs devtmpfs rw",
            "7 3 0:3 /sys /proc/sys ro - proc proc rw",
        ]
        .join("\n")
    }

    fn good(step: bool) -> String {
        let caps = if step { NO_CAPS } else { SERVICE_CAP_MASK };
        let id = if step { "1000" } else { "0" };
        format!(
            "uid={id}\ngid={id}\nnet=lo\nnc=present\nCapPrm={caps}\nCapEff={caps}\nCapBnd={caps}\nNoNewPrivs=1\n\
             memory_max=2147483648\npids_max=512\ncpu_max=200000/100000\nuid_map=0/1100/1\n---mountinfo\n{}",
            mounts()
        )
    }

    #[test]
    fn a_clean_probe_passes_both_profiles() {
        assert_eq!(
            judge(&Probe::parse(&good(true)), Profile::Step, JOB, &CEILING),
            Ok(())
        );
        assert_eq!(
            judge(&Probe::parse(&good(false)), Profile::Service, JOB, &CEILING),
            Ok(())
        );
    }

    #[test]
    fn each_breach_fails_closed_with_its_check() {
        let cases: Vec<(String, Profile, &str)> = vec![
            (
                good(true).replace("net=lo", "net=lo eth0"),
                Profile::Step,
                "network",
            ),
            (
                good(true).replace("nc=present\n", ""),
                Profile::Step,
                "network",
            ),
            (good(true) + "\n", Profile::Step, "ok"),
            (
                good(true).replace("nc=present", "nc=present\ntcp=connected"),
                Profile::Step,
                "network",
            ),
            (
                good(true).replace("uid=1000\n", "uid=1000\nhost_path=/Users\n"),
                Profile::Step,
                "host_paths",
            ),
            (
                good(true).replace("uid=1000\n", "uid=1000\nmac_cmd=/opt/x/macctl\n"),
                Profile::Step,
                "mac_commands",
            ),
            (
                good(true).replace("CapEff=0000000000000000", "CapEff=0000000000002000"),
                Profile::Step,
                "capabilities",
            ),
            (good(false), Profile::Step, "user"),
            (good(true), Profile::Service, "capabilities"),
            (
                good(true).replace("NoNewPrivs=1", "NoNewPrivs=0"),
                Profile::Step,
                "capabilities",
            ),
            (
                good(true).replace("uid_map=0/1100/1", "uid_map=0/0/4294967295"),
                Profile::Step,
                "user",
            ),
            (
                good(true).replace("NoNewPrivs=1", "NoNewPrivs=1\nroot=writable"),
                Profile::Step,
                "root",
            ),
            (
                good(true).replace("size=524288k", "size=1g"),
                Profile::Step,
                "root",
            ),
            (
                good(true).replace("pids_max=512", "pids_max=max"),
                Profile::Step,
                "limits",
            ),
            (
                good(true).replace("cpu_max=200000/100000", "cpu_max=max/100000"),
                Profile::Step,
                "limits",
            ),
            (
                good(true) + "\n8 1 0:9 /Users/oreo /Users rw - virtiofs mac rw",
                Profile::Step,
                "mounts",
            ),
            (
                good(true) + "\n8 1 0:9 / /mnt/x rw - fuse.sshfs s rw",
                Profile::Step,
                "mounts",
            ),
            (
                good(true) + "\n8 1 0:38 /scon/c/rootfs/tmp /data rw - btrfs /dev/vdb1 rw",
                Profile::Step,
                "mounts",
            ),
            (
                good(true) + "\n8 1 0:1 / /etc/argus ro - tmpfs t rw",
                Profile::Step,
                "mounts",
            ),
            ("nothing".into(), Profile::Step, "probe"),
        ];
        for (out, profile, want) in cases {
            let r = judge(&Probe::parse(&out), profile, JOB, &CEILING);
            if want == "ok" {
                assert_eq!(r, Ok(()));
            } else {
                let e = r.expect_err(want);
                assert!(
                    e.starts_with(&format!("container_isolation_failed:{want}")),
                    "{want}: {e}"
                );
            }
        }
    }

    #[test]
    fn inspect_allows_only_the_jobs_named_volumes_and_no_network() {
        let vol = serde_json::json!([{ "Mounts": [{ "Type": "volume", "Name": "wgsbx-j-00000000000000aa-src" }] }]);
        assert_eq!(judge_inspect(&vol, JOB), Ok(()));
        let bind =
            serde_json::json!([{ "Mounts": [{ "Type": "bind", "Source": "/tmp", "Name": "" }] }]);
        assert!(judge_inspect(&bind, JOB)
            .unwrap_err()
            .contains("bind mount of /tmp"));
        let other = serde_json::json!({ "Mounts": [{ "Type": "volume", "Name": "argus-data" }] });
        assert!(judge_inspect(&other, JOB).is_err());
        assert!(judge_inspect(&serde_json::json!({}), JOB).is_err());
        assert_eq!(
            judge_network(&serde_json::json!([{ "HostConfig": { "NetworkMode": "none" } }])),
            Ok(())
        );
        assert!(
            judge_network(&serde_json::json!({ "HostConfig": { "NetworkMode": "pasta" } }))
                .is_err()
        );
    }
}
