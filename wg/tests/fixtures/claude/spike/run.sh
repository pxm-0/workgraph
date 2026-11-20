#!/bin/bash
# usage: run.sh <case> <prompt> [claude args...]   (cwd = temp repo; env cleared like §5.1)
S=$(cd "$(dirname "$0")" && pwd)
case=$1; prompt=$2; shift 2
cd "$S/${REPO:-repo}" || exit 1
printf '%s' "$prompt" | env -i HOME="$HOME" USER="$USER" LANG=en_US.UTF-8 TERM=dumb \
  PATH=/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin TMPDIR="$S/tmp" WG_AGENT=1 ${EXTRA_ENV} \
  /opt/homebrew/bin/claude -p --no-session-persistence "$@" >"$S/raw/$case.json" 2>"$S/raw/$case.stderr"
echo "exit=$?" > "$S/raw/$case.exit"
cat "$S/raw/$case.exit"
printf '%s\n' "$*" > "$S/raw/$case.argv"
