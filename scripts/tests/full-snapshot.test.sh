#!/usr/bin/env bash
# Automated contract tests for scripts/full-snapshot.sh: pure-bash assertions
# over the CLI's pre-Docker logic (usage, name validation, guard rails, dir
# isolation from scripts/snapshot.sh) plus one save fixture with a stubbed
# `docker` binary. No live Docker/Compose stack is used or required -- see
# docs/SNAPSHOTS.md's "Full snapshots" section for the manual, Docker-backed
# integration checks this suite does not attempt.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
FULL_SNAPSHOT="$REPO_ROOT/scripts/full-snapshot.sh"
SNAPSHOT="$REPO_ROOT/scripts/snapshot.sh"

pass_count=0
fail_count=0

pass() { pass_count=$((pass_count + 1)); echo "  ok - $1"; }
fail() { fail_count=$((fail_count + 1)); echo "  FAIL - $1: $2"; }

assert_eq() { # assert_eq LABEL EXPECTED ACTUAL
    if [ "$2" = "$3" ]; then pass "$1"; else fail "$1" "expected '$2', got '$3'"; fi
}

assert_contains() { # assert_contains LABEL HAYSTACK NEEDLE
    if [[ "$2" == *"$3"* ]]; then pass "$1"; else fail "$1" "expected output to contain '$3', got: $2"; fi
}

work_dir=""
setup() { work_dir="$(mktemp -d)"; }
teardown() { [ -n "$work_dir" ] && rm -rf "$work_dir"; }

echo "T1: no args -> usage + exit 1"
setup
out="$("$FULL_SNAPSHOT" 2>&1)" && rc=0 || rc=$?
assert_eq "T1 exit code" 1 "$rc"
assert_contains "T1 usage text" "$out" "Usage:"
teardown

echo "T2: save 'bad name!' -> name-regex error, non-zero, no files written"
setup
out="$(FULL_SNAPSHOT_DIR="$work_dir" "$FULL_SNAPSHOT" save 'bad name!' 2>&1)" && rc=0 || rc=$?
assert_eq "T2 exit code" 1 "$rc"
assert_contains "T2 error text" "$out" "must match"
assert_eq "T2 no files written" "0" "$(find "$work_dir" -type f | wc -l | tr -d ' ')"
teardown

echo "T3: restore does-not-exist (empty dir) -> no-such-snapshot error, non-zero"
setup
out="$(FULL_SNAPSHOT_DIR="$work_dir" "$FULL_SNAPSHOT" restore does-not-exist 2>&1)" && rc=0 || rc=$?
assert_eq "T3 exit code" 1 "$rc"
assert_contains "T3 error text" "$out" "no such snapshot"
teardown

echo "T4: save foo when foo files pre-exist -> already-exists error, non-zero, exists-check precedes docker"
setup
: > "$work_dir/foo.tar.gz"
: > "$work_dir/foo.json"
out="$(FULL_SNAPSHOT_DIR="$work_dir" PATH="/nonexistent" "$FULL_SNAPSHOT" save foo 2>&1)" && rc=0 || rc=$?
assert_eq "T4 exit code" 1 "$rc"
assert_contains "T4 error text" "$out" "already exists"
teardown

echo "T5: list on empty dir -> no-snapshots-yet message, exit 0"
setup
out="$(FULL_SNAPSHOT_DIR="$work_dir" "$FULL_SNAPSHOT" list 2>&1)" && rc=0 || rc=$?
assert_eq "T5 exit code" 0 "$rc"
assert_contains "T5 message" "$out" "no snapshots yet"
teardown

echo "T6: dir isolation -- full-snapshot list and snapshot.sh list don't see each other's snapshots"
setup
plain_dir="$work_dir/plain"
full_dir="$work_dir/full"
mkdir -p "$plain_dir" "$full_dir"
cat > "$plain_dir/plain-snap.json" <<'EOF'
{
  "name": "plain-snap",
  "created": "2026-01-01T00:00:00Z",
  "height": 1,
  "best_block_hash": "aaaa",
  "btc_image": "x",
  "node2_wallet": "node2",
  "node3_wallet": "node3",
  "user_address": "addr",
  "node1_disable_wallet": "1",
  "services": "btc-simnet-node1"
}
EOF
: > "$plain_dir/plain-snap.tar.gz"
cat > "$full_dir/full-snap.json" <<'EOF'
{
  "name": "full-snap",
  "created": "2026-01-01T00:00:00Z",
  "height": 1,
  "best_block_hash": "bbbb",
  "btc_image": "x",
  "node2_wallet": "node2",
  "node3_wallet": "node3",
  "user_address": "addr",
  "node1_disable_wallet": "1",
  "services": "btc-simnet-node1",
  "electrs": true,
  "mempool_db": false
}
EOF
: > "$full_dir/full-snap.tar.gz"
full_out="$(FULL_SNAPSHOT_DIR="$full_dir" "$FULL_SNAPSHOT" list 2>&1)"
plain_out="$(SNAPSHOT_DIR="$plain_dir" "$SNAPSHOT" list 2>&1)"
if [[ "$full_out" == *"full-snap"* && "$full_out" != *"plain-snap"* ]]; then
    pass "T6 full-snapshot list shows only full-snap"
else
    fail "T6 full-snapshot list shows only full-snap" "got: $full_out"
fi
if [[ "$plain_out" == *"plain-snap"* && "$plain_out" != *"full-snap"* ]]; then
    pass "T6 snapshot.sh list shows only plain-snap"
else
    fail "T6 snapshot.sh list shows only plain-snap" "got: $plain_out"
fi
teardown

echo "T7: scripts/snapshot.sh is byte-for-byte unchanged (AC1 guard)"
base_ref="origin/${GITHUB_BASE_REF:-master}"
if git -C "$REPO_ROOT" rev-parse --verify --quiet "$base_ref" >/dev/null; then
    merge_base="$(git -C "$REPO_ROOT" merge-base HEAD "$base_ref")"
    diff_out="$(git -C "$REPO_ROOT" diff --exit-code "$merge_base" -- scripts/snapshot.sh 2>&1)" && rc=0 || rc=$?
    assert_eq "T7 snapshot.sh unchanged since merge-base with $base_ref" 0 "$rc"
    [ "$rc" -eq 0 ] || echo "$diff_out"
else
    fail "T7 snapshot.sh unchanged since merge-base with $base_ref" \
        "'$base_ref' does not resolve; fetch it (e.g. 'git fetch origin ${GITHUB_BASE_REF:-master}') and rerun"
fi

echo "T8: save fixture (stubbed docker) writes electrs/mempool_db flags correctly"
setup
stub_bin="$work_dir/stub-bin"
mkdir -p "$stub_bin"
cat > "$stub_bin/docker" <<'STUB'
#!/usr/bin/env bash
# Fakes only the docker/docker-compose calls scripts/full-snapshot.sh's
# cmd_save makes, so the metadata-writing logic can be exercised without a
# real Docker daemon.
set -euo pipefail
case "$1" in
  inspect)
    echo true
    ;;
  exec)
    if [[ "$*" == *getblockcount* ]]; then
      echo 200
    elif [[ "$*" == *getbestblockhash* ]]; then
      echo cafebabe00000000000000000000000000000000000000000000000000dead
    fi
    ;;
  compose)
    shift
    args="$*"
    if [[ "$args" == *"ps --services --status running"* ]]; then
      printf 'btc-simnet-node1\nelectrs\n'
    fi
    # stop/start/down/create/up: no-op success
    ;;
  run)
    shift
    out_dir=""
    tar_target=""
    for a in "$@"; do
      case "$a" in
        *:/out) out_dir="${a%:/out}" ;;
        /out/*.tar.gz) tar_target="${a#/out/}" ;;
      esac
    done
    if [ -n "$out_dir" ] && [ -n "$tar_target" ]; then
      : > "$out_dir/$tar_target"
    fi
    ;;
  cp)
    last="${!#}"
    if [ "$last" = "-" ]; then
      printf 'FAKE-ELECTRS-TAR-DATA'
    else
      cat >/dev/null
    fi
    ;;
esac
STUB
chmod +x "$stub_bin/docker"

out="$(FULL_SNAPSHOT_DIR="$work_dir" PATH="$stub_bin:$PATH" "$FULL_SNAPSHOT" save t8run 2>&1)" && rc=0 || rc=$?
assert_eq "T8 save exit code" 0 "$rc"
meta="$work_dir/t8run.json"
if [ -f "$meta" ]; then
    pass "T8 metadata file written"
    electrs_flag="$(sed -n 's/^  "electrs": \([a-z]*\),\{0,1\}$/\1/p' "$meta")"
    mempool_db_flag="$(sed -n 's/^  "mempool_db": \([a-z]*\)$/\1/p' "$meta")"
    assert_eq "T8 electrs flag true (service was in running list)" "true" "$electrs_flag"
    assert_eq "T8 mempool_db flag false (service was not in running list)" "false" "$mempool_db_flag"
    if [ -f "$work_dir/t8run.electrs.tar" ]; then
        pass "T8 electrs store archived"
    else
        fail "T8 electrs store archived" "$work_dir/t8run.electrs.tar missing"
    fi
    if [ ! -f "$work_dir/t8run.mempool-db.tar" ]; then
        pass "T8 mempool-db store not archived (not running at save time)"
    else
        fail "T8 mempool-db store not archived (not running at save time)" "unexpected $work_dir/t8run.mempool-db.tar"
    fi
else
    fail "T8 metadata file written" "$meta missing; save output: $out"
fi
teardown

echo
echo "full-snapshot.test.sh: $pass_count passed, $fail_count failed"
[ "$fail_count" -eq 0 ]
