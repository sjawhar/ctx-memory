#!/bin/sh
# Drives a running ctx server through the surface an agent uses: the health endpoint, a read
# command in remote mode, and - with --write - one record written and removed again.
#
# Usage:
#   CTX_SERVER='http://ctx-memory.<tailnet>.ts.net' scripts/remote-smoke.sh [--write] [query]
#
# CTX_SERVER is required: the script names no server of its own and stops before any request
# without one. On a dev box the tailnet says who is calling and no token is needed. Inside
# Google Cloud, set CTX_SERVER to the Cloud Run URL and CTX_SERVER_AUDIENCE to the same value,
# and from a Legion pod also set CTX_IMPERSONATE_SA.
set -eu

write=0
if [ "${1-}" = "--write" ]; then
    write=1
    shift
fi
query=${1-renewal}

: "${CTX_SERVER:?set CTX_SERVER to the server URL, for example http://ctx-memory.<tailnet>.ts.net}"
ctx=${CTX_BIN:-ctx}

echo "server: $CTX_SERVER"

echo "--- healthz"
curl -fsS "$CTX_SERVER/healthz"
echo

echo "--- ctx rules (first line)"
"$ctx" rules | head -n 1

echo "--- ctx search $query"
"$ctx" search "$query" | head -n 12

echo "--- ctx pending"
"$ctx" pending

if [ "$write" -eq 0 ]; then
    echo "read-only smoke finished; pass --write to also write a record"
    exit 0
fi

stamp=$(date -u +%Y%m%dT%H%M%SZ)
path="smoke/remote-$stamp.md"
echo "--- ctx record put $path"
"$ctx" record put "$path" - <<EOF
---
type: note
title: Remote smoke $stamp
---
Written by scripts/remote-smoke.sh to prove the write path, and removed again.
EOF

echo "--- ctx record rm $path"
"$ctx" record rm "$path"

echo "write round trip finished"
