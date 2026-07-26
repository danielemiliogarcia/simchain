#!/usr/bin/env bash

# Check the filtered Compose-managed Bitcoin config allowlist. Runtime HTTP
# behavior is covered separately by check-node1-rpc-policy-live.sh.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

die() {
    echo "node1 RPC policy check: $*" >&2
    exit 1
}

cd "$repo_root"
policy_file="$repo_root/docker/node1-rpc-configs.compose.yml"
[ -r "$policy_file" ] || die "missing policy file: $policy_file"
grep -Fxq '  node1-rpc-true:' "$policy_file" \
    || die "filtered config definition is missing"
grep -Fxq '  node1-rpc-false:' "$policy_file" \
    || die "unfiltered config definition is missing"
rendered_policy="$(FILTER_NODE1_RPC=true docker compose config --format json \
    | jq -er '.configs["node1-rpc-true"].content')" \
    || die "could not render the filtered Compose config"
grep -Fxq 'rpcwhitelistdefault=0' <<<"$rendered_policy" \
    || die "policy must leave authenticated users without an explicit rule unrestricted"
grep -Eq '^rpcauth=simchain-internal:[^$]+\$+[0-9a-f]{64}$' <<<"$rendered_policy" \
    || die "internal rpcauth identity is missing or malformed"
allowlist="$(sed -n 's/^rpcwhitelist=[^:]*://p' <<<"$rendered_policy")"
[ -n "$allowlist" ] || die "rendered node1 allowlist is empty"

mapfile -t methods < <(tr ', ' '\n\n' <<<"$allowlist" | sed '/^$/d')
[ "${#methods[@]}" -eq 149 ] || die "expected 149 allowed Core 31.1 methods, got ${#methods[@]}"
[ "$(printf '%s\n' "${methods[@]}" | LC_ALL=C sort -u | wc -l)" -eq 149 ] \
    || die "allowlist contains duplicate methods"
printf '%s\n' "${methods[@]}" \
    | awk 'NF != 1 || $1 !~ /^[a-z][a-z0-9]*$/ { exit 1 }' \
    || die "allowlist contains an invalid method name"

contains() {
    printf '%s\n' "${methods[@]}" | grep -Fxq "$1"
}

denied=(
    generate generateblock generatetoaddress generatetodescriptor
    mockscheduler setmocktime syncwithvalidationinterfacequeue
    abortprivatebroadcast clearbanned disconnectnode getblockfrompeer
    invalidateblock preciousblock prioritisetransaction pruneblockchain
    reconsiderblock setban setnetworkactive stop submitblock submitheader
)
exceptions=(
    addconnection addnode addpeeraddress sendmsgtopeer
    echo echojson echoipc logging
    dumptxoutset loadtxoutset savemempool importmempool
)

for method in "${denied[@]}"; do
    ! contains "$method" || die "denied method '$method' is present in the allowlist"
done
for method in "${exceptions[@]}"; do
    contains "$method" || die "intentional exception '$method' is missing from the allowlist"
done

for example in .env.example .env.full.example; do
    ! grep -Eq '^NODE1_INTERNAL_RPC_(USER|PASS|AUTH)=' "$example" \
        || die "internal RPC wiring must not be exposed in $example"
done

echo "Node1 RPC policy verified (restricted public user, full-access internal identity, declarative 149-method Core 31.1 allowlist)"
