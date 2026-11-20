//! `wg sandbox setup-print` (design §8.6): a plan and a script the OWNER runs after one yes. The
//! factory never runs it. It provisions the existing machine only: it refuses unless `orbctl list`
//! shows it running, and never touches argus, Docker, nft, Tailscale, Caddy or the golden image.

use crate::config::Config;

/// Every image the agent may run: each repo's image and services, plus the doctor's probe.
pub fn images(cfg: &Config) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for r in cfg.repos.iter().filter_map(|r| r.sandbox.as_ref()) {
        for i in std::iter::once(r.image.clone()).chain(r.services().into_iter().map(|s| s.image)) {
            if !out.contains(&i) {
                out.push(i);
            }
        }
    }
    out
}

/// `/etc/wgsbx/images.toml`, from the owner's `factory.toml` digests only.
pub fn allowlist(cfg: &Config) -> String {
    let list: Vec<String> = images(cfg).iter().map(|i| format!("  \"{i}\",")).collect();
    format!(
        "probe = \"{}\"\nimages = [\n{}\n]\n",
        cfg.sandbox.probe,
        list.join("\n")
    )
}

const PROVISION: &str = r#"set -eu
export DEBIAN_FRONTEND=noninteractive
apt-get update -q
apt-get install -y -q podman uidmap passt systemd-container
id wgsbx >/dev/null 2>&1 || useradd --uid 1100 --create-home --shell /bin/bash wgsbx
passwd -l wgsbx
# A subuid range past every existing one (argus's sandbox users have their own): never shared.
for f in /etc/subuid /etc/subgid; do
  grep -q '^wgsbx:' "$f" || awk -F: 'BEGIN{m=100000} {e=$2+$3; if (e>m) m=e} END{print "wgsbx:" m ":65536"}' "$f" >> "$f"
done
loginctl enable-linger wgsbx
install -d -o root -g root -m 0755 /usr/local/lib/wgsbx /etc/wgsbx
for i in 1 2 3 4 5 6 7 8 9 10; do [ -d /run/user/1100 ] && break; sleep 1; done
[ -d /run/user/1100 ] || { echo "no /run/user/1100: lingering did not start the user session" >&2; exit 1; }
grep -qw memory /sys/fs/cgroup/user.slice/user-1100.slice/user@1100.service/cgroup.controllers || { echo "cgroup v2 does not delegate memory to wgsbx" >&2; exit 1; }
"#;

/// The plan and the script, as text for the owner.
pub fn print(cfg: &Config, version: &str) -> String {
    let m = &cfg.sandbox.machine;
    let orb = &cfg.orb_bin;
    let pulls: Vec<String> = images(cfg)
        .iter()
        .chain([&cfg.sandbox.probe])
        .map(|i| format!("\"$ORB\" run -m \"$M\" -u wgsbx podman pull -q '{i}'"))
        .collect();
    format!(
        r#"PLAN (you run this, once, after saying yes; workgraph never runs it)
- Machine: {m}, which must already be running. Nothing is created, reset, deleted, started or stopped.
- Installs on {m}: podman uidmap passt systemd-container; user wgsbx (uid 1100, /bin/bash, password locked,
  lingering on, its own subuid/subgid range); root-owned /usr/local/lib/wgsbx/wg (the agent, wg {version}, sha256
  checked) and /etc/wgsbx/images.toml (the digests below); pulls those images as wgsbx.
- Never touches /etc/argus, /srv/argus, argus units, Docker, nft, Tailscale, Caddy, infra/server-sim/sim or the
  -golden machine. A reset of {m} from its golden image removes all of this (doctor then says not_provisioned).
- Accepted residual risk (design §17 D1): an escape from a container lands as wgsbx in the VM.

SCRIPT
set -eu
ORB='{orb}'; M='{m}'
orbctl list | awk -v m="$M" '$1 == m && $2 == "running" {{ ok = 1 }} END {{ exit !ok }}' \
  || {{ echo "$M is not running; start it yourself first" >&2; exit 1; }}
"$ORB" run -m "$M" -u root sh -s <<'PROVISION'
{PROVISION}PROVISION
# The agent: WG_AGENT_BIN (a local linux x86_64 wg, e.g. a PR's CI artifact) or the v{version} release asset.
if [ -z "${{WG_AGENT_BIN:-}}" ]; then
  T=$(mktemp -d); gh release download v{version} -R pxm-0/workgraph -p 'wg-x86_64-unknown-linux-gnu*' -D "$T"
  (cd "$T" && shasum -a 256 -c wg-x86_64-unknown-linux-gnu.sha256); WG_AGENT_BIN="$T/wg-x86_64-unknown-linux-gnu"
fi
SUM=$(shasum -a 256 "$WG_AGENT_BIN" | cut -d' ' -f1)
"$ORB" run -m "$M" -u root sh -c 'cat > /usr/local/lib/wgsbx/wg.new && chown root:root /usr/local/lib/wgsbx/wg.new && chmod 0755 /usr/local/lib/wgsbx/wg.new && mv /usr/local/lib/wgsbx/wg.new /usr/local/lib/wgsbx/wg' < "$WG_AGENT_BIN"
"$ORB" run -m "$M" -u root sha256sum /usr/local/lib/wgsbx/wg | grep -q "^$SUM " || {{ echo "agent checksum mismatch" >&2; exit 1; }}
"$ORB" run -m "$M" -u root sh -c 'umask 022; cat > /etc/wgsbx/images.toml' <<'IMAGES'
{allow}IMAGES
{pulls}
wg sandbox doctor
"#,
        allow = allowlist(cfg),
        pulls = pulls.join("\n"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_script_provisions_only_and_lists_every_digest() {
        let d = "c".repeat(64);
        let text = format!("[[repo]]\nname = \"o/r\"\npath = \"/r\"\n[repo.sandbox]\nimage = \"docker.io/library/node@sha256:{d}\"\nservices = {{ db = {{ image = \"docker.io/library/postgres@sha256:{d}\" }} }}\n");
        let cfg = crate::config::parse(&text).unwrap();
        let s = print(&cfg, "0.4.0");
        for want in [
            "useradd --uid 1100",
            "passwd -l wgsbx",
            "loginctl enable-linger wgsbx",
            "podman uidmap passt systemd-container",
            "chown root:root /usr/local/lib/wgsbx/wg.new",
            "/etc/wgsbx/images.toml",
            "node@sha256",
            "postgres@sha256",
            "busybox@sha256",
            "wg sandbox doctor",
            "is not running",
        ] {
            assert!(s.contains(want), "{want}");
        }
        for never in [
            "/etc/argus\n",
            "orbctl create",
            "orbctl delete",
            "orb delete",
            "orbctl stop",
            "systemctl stop argus",
            "golden'",
            "sudo ",
        ] {
            assert!(!s.contains(never), "{never}");
        }
        let a = allowlist(&cfg);
        let parsed = crate::sandbox::agent::store::Allowlist::parse(&a).unwrap();
        assert_eq!(parsed.images.len(), 2);
    }
}
