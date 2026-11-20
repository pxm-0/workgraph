#!/bin/sh
# The real-sandbox drill (design §15.4 "Real sandbox", §16 Phase 3 manual checklist), run on the Mac
# against the provisioned oreochiserver-sim. The Mac has no cargo, so it drives a prebuilt macOS wg
# (a PR's CI artifact or a release asset) instead of `cargo test -- --ignored sim_`:
#
#   WG=/path/to/macos/wg [PLANT_AGENT=/usr/local/lib/wgsbx/wg-plant-test] sh wg/tests/sim/drill.sh
#
# It uses a throwaway HOME and repo, never the owner's ~/.workgraph. It runs: the doctor; the argus
# fingerprint before and after; 20 jobs through `wg sandbox exec --task --check`, each one a
# container-isolation check, with 0 leftovers after; a migration rehearsal whose generated file
# comes back as a patch limited to the unit's files; and, with PLANT_AGENT (a root-owned debug
# build of the agent), the doctor failing closed on each planted breach. It never starts, stops,
# resets or deletes the machine and never touches /etc/argus.
set -eu
WG=${WG:?set WG to a macOS wg binary}
M=oreochiserver-sim
IMAGE=docker.io/alpine/git@sha256:c0280cf9572316299b08544065d3bf35db65043d5e3963982ec50647d2746e26
T=$(mktemp -d)
export HOME="$T/home"
mkdir -p "$HOME/.workgraph"
pass() { printf 'PASS %s\n' "$1"; }
fail() { printf 'FAIL %s\n' "$1"; exit 1; }

repo="$T/repo"
mkdir -p "$repo/.haruspex/tasks"
cd "$repo"
git init -q -b main
git config user.email drill@example.invalid
git config user.name drill
echo hi > hello.txt
printf 'mkdir -p db/migrations\nprintf "CREATE TABLE t (id int);\\n" > db/migrations/001_t.sql\n' > migrate.sh
echo '{}' > .haruspex/tasks/T-001.json
git add -A && git commit -qm init

cat > "$HOME/.workgraph/factory.toml" <<EOF
[[repo]]
name = "sim/drill"
path = "$repo"
[repo.sandbox]
image = "$IMAGE"
checks = { isolation = "test ! -e /Users && test ! -e /etc/argus && test ! -e /srv/argus && test \"\$(ls /sys/class/net)\" = lo && ! wget -q -T 3 -O /dev/null http://example.com && test \"\$(id -u)\" = 1000 && ! touch /x 2>/dev/null && cat hello.txt" }
EOF

fp() { orb run -m "$M" systemctl list-unit-files 'argus*' --no-legend; echo ---; orb run -m "$M" systemctl list-units 'argus*' --state=failed --no-legend; }
before=$(fp | shasum -a 256)

"$WG" sandbox doctor > "$T/doctor.json" && pass "doctor ok" || { cat "$T/doctor.json"; fail "doctor"; }
grep -q '"vm_host_reach":"accepted"' "$T/doctor.json" && pass "vm_host_reach stated as accepted"

i=1
while [ $i -le 20 ]; do
  "$WG" sandbox exec --task T-001 --check isolation > "$T/job-$i.txt" || { cat "$T/job-$i.txt"; fail "job $i"; }
  i=$((i + 1))
done
pass "20 jobs: no /Users, no /etc/argus, only lo, no egress, uid 1000, read-only root"
left=$(orb run -m "$M" -u wgsbx sh -c 'export XDG_RUNTIME_DIR=/run/user/$(id -u); podman ps -a --filter label=wg.job -q | wc -l; podman volume ls --filter label=wg.job -q | wc -l' | tr '\n' ' ')
[ "$left" = "0 0 " ] && pass "0 leftovers after 20 jobs" || fail "leftovers: $left"

mkdir -p "$HOME/.workgraph/runs/r-drill/guard"
cat > "$HOME/.workgraph/runs/r-drill/guard/m.manifest.json" <<EOF
{"label":"work:m","log":"$HOME/.workgraph/runs/r-drill/guard.jsonl","worktree":"$repo","read_roots":[],"write_roots":[],
 "secret_roots":[],"tools":[],"skills":[],"bash":{"exact":[],"prefixes":[],"haruspex_cli":null,"haruspex":[],"task_id":null,"switch_branch":false},
 "sandbox":{"repo":"sim/drill","run":"r-drill","unit":"m","task":null,"files":["db/migrations/"]}}
EOF
WG_GUARD_MANIFEST="$HOME/.workgraph/runs/r-drill/guard/m.manifest.json" "$WG" sandbox exec --unit m --patch -- sh migrate.sh > "$T/patch.json" || true
grep -q '"applied":\["db/migrations/001_t.sql"\]' "$T/patch.json" && [ -f "$repo/db/migrations/001_t.sql" ] \
  && pass "migration rehearsal: the generated file came back as a patch within the unit's files" || { cat "$T/patch.json"; fail "patch"; }

if [ -n "${PLANT_AGENT:-}" ]; then
  for p in bind macctl network cap; do
    orb run -m "$M" -u wgsbx env "WGSBX_TEST_PLANT=$p" "$PLANT_AGENT" sandbox-agent doctor > "$T/plant-$p.json" || true
    grep -q '"ok":false' "$T/plant-$p.json" && grep -q 'container_isolation_failed' "$T/plant-$p.json" \
      && pass "doctor fails closed on a planted $p" || { cat "$T/plant-$p.json"; fail "plant $p"; }
  done
fi

after=$(fp | shasum -a 256)
[ "$before" = "$after" ] && pass "argus fingerprint unchanged" || fail "argus fingerprint changed"
echo "drill done; scratch in $T"
