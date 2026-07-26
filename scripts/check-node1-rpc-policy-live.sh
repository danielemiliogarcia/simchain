#!/usr/bin/env bash

# Exercise node1's native Bitcoin Core RPC whitelist over the real HTTP
# endpoint. The default strict policy and a running basic stack are required.
# Negative probes use invalid argument counts where an accidentally allowed
# administrative call could otherwise mutate state. The final positive node2
# check intentionally mines one block to prove that node2 is fully unaffected.

set -euo pipefail

node1_container=btc-simnet-node1
node2_container=btc-simnet-node2
response_file="$(mktemp)"
trap 'unlink "$response_file" 2>/dev/null || true' EXIT

die() {
    echo "node1 live RPC policy check: $*" >&2
    exit 1
}

for command in curl docker jq; do
    command -v "$command" >/dev/null 2>&1 || die "required command is unavailable: $command"
done

for container in "$node1_container" "$node2_container"; do
    [ "$(docker inspect -f '{{.State.Running}}' "$container" 2>/dev/null || true)" = "true" ] \
        || die "$container is not running (start the basic stack first)"
done

container_env() {
    local container="$1" name="$2"
    docker inspect --format '{{range .Config.Env}}{{println .}}{{end}}' "$container" \
        | sed -n "s/^${name}=//p" | tail -n 1
}

published_rpc_port() {
    local container="$1"
    docker inspect "$container" \
        | jq -r '.[0].NetworkSettings.Ports["18443/tcp"][0].HostPort // empty'
}

rpc_user="$(container_env "$node1_container" BTC_RPC_USER)"
rpc_pass="$(container_env "$node1_container" BTC_RPC_PASS)"
node1_port="$(published_rpc_port "$node1_container")"
node2_port="$(published_rpc_port "$node2_container")"

[ -n "$rpc_user" ] || die "could not resolve BTC_RPC_USER from node1"
[ -n "$rpc_pass" ] || die "could not resolve BTC_RPC_PASS from node1"
[ -n "$node1_port" ] || die "node1 RPC port is not published to the host"
[ -n "$node2_port" ] || die "node2 RPC port is not published to the host"

RPC_HTTP=""
RPC_BODY=""
rpc() {
    local port="$1" payload="$2" path="${3:-/}"
    RPC_HTTP="$(curl -sS \
        --connect-timeout 5 \
        --max-time 30 \
        --user "$rpc_user:$rpc_pass" \
        --header 'content-type: application/json' \
        --data-binary "$payload" \
        --output "$response_file" \
        --write-out '%{http_code}' \
        "http://127.0.0.1:${port}${path}")" \
        || die "RPC transport failed on port $port"
    RPC_BODY="$(<"$response_file")"
}

assert_allowed() {
    local method="$1" payload="$2"
    rpc "$node1_port" "$payload"
    [ "$RPC_HTTP" != "403" ] \
        || die "expected '$method' to be allowed, but node1 returned HTTP 403"
}

assert_denied() {
    local method="$1" path="${2:-/}"
    # Twenty arguments exceed every denied method's accepted arity. If a
    # method accidentally becomes allowed, Core returns a harmless parameter
    # error instead of executing it.
    local payload
    payload="$(jq -nc --arg method "$method" \
        '{jsonrpc:"2.0",id:1,method:$method,params:[null,null,null,null,null,null,null,null,null,null,null,null,null,null,null,null,null,null,null,null]}')"
    rpc "$node1_port" "$payload" "$path"
    [ "$RPC_HTTP" = "403" ] \
        || die "expected '$method' to be denied, got HTTP $RPC_HTTP: $RPC_BODY"
    [ -z "$RPC_BODY" ] \
        || die "expected Core's empty 403 body for '$method', got: $RPC_BODY"
}

# Ordinary application-facing reads and raw-transaction submission reach Core.
assert_allowed getblockcount \
    '{"jsonrpc":"2.0","id":1,"method":"getblockcount","params":[]}'
jq -e '.error == null and (.result | type == "number")' <<<"$RPC_BODY" >/dev/null \
    || die "getblockcount returned an unexpected response: $RPC_BODY"

assert_allowed sendrawtransaction \
    '{"jsonrpc":"2.0","id":2,"method":"sendrawtransaction","params":["00"]}'

# Every documented connectivity and advanced-testing exception must pass the
# whitelist. Missing required parameters are intentional for mutation-capable
# methods: reaching Core's JSON error proves authorization without changing it.
exceptions=(
    addconnection addnode addpeeraddress sendmsgtopeer
    echo echojson echoipc logging
    dumptxoutset loadtxoutset savemempool importmempool
)
for method in "${exceptions[@]}"; do
    assert_allowed "$method" \
        "$(jq -nc --arg method "$method" '{jsonrpc:"2.0",id:3,method:$method,params:[]}')"
done

# Exercise every denied method, not just representative samples.
denied=(
    generate generateblock generatetoaddress generatetodescriptor
    mockscheduler setmocktime syncwithvalidationinterfacequeue
    abortprivatebroadcast clearbanned disconnectnode getblockfrompeer
    invalidateblock preciousblock prioritisetransaction pruneblockchain
    reconsiderblock setban setnetworkactive stop submitblock submitheader
)
for method in "${denied[@]}"; do
    assert_denied "$method"
done

# Wallet routing, whitespace, and named parameters must not bypass method
# authorization. This also uses an intentionally invalid named argument.
rpc "$node1_port" \
    ' { "jsonrpc":"2.0", "id":4, "method":"setmocktime", "params":{"__simchain_policy_probe":true} } ' \
    '/wallet/not-loaded'
[ "$RPC_HTTP" = "403" ] && [ -z "$RPC_BODY" ] \
    || die "wallet-path/named-parameter probe bypassed the policy"

# Bitcoin Core prechecks every batch member. Attempt a visible logging-state
# mutation beside a forbidden call, then prove the allowed member never ran.
rpc "$node1_port" \
    '{"jsonrpc":"2.0","id":5,"method":"logging","params":[]}'
[ "$RPC_HTTP" != "403" ] || die "logging exception is unexpectedly denied"
category="$(jq -r '.result | to_entries | map(select(.key != "all")) | .[0].key // empty' <<<"$RPC_BODY")"
[ -n "$category" ] || die "Core returned no logging category for atomicity probe"
before="$(jq -r --arg category "$category" '.result[$category]' <<<"$RPC_BODY")"
if [ "$before" = "true" ]; then
    logging_params="$(jq -nc --arg category "$category" '[[],[$category]]')"
else
    logging_params="$(jq -nc --arg category "$category" '[[ $category ],[]]')"
fi
batch="$(jq -nc --argjson params "$logging_params" \
    '[{jsonrpc:"2.0",id:6,method:"logging",params:$params},{jsonrpc:"2.0",id:7,method:"setmocktime",params:[0]}]')"
rpc "$node1_port" "$batch"
[ "$RPC_HTTP" = "403" ] && [ -z "$RPC_BODY" ] \
    || die "mixed allowed/denied batch was not rejected atomically"
rpc "$node1_port" \
    '{"jsonrpc":"2.0","id":8,"method":"logging","params":[]}'
after="$(jq -r --arg category "$category" '.result[$category]' <<<"$RPC_BODY")"
[ "$after" = "$before" ] \
    || die "allowed batch member executed despite forbidden sibling ($category: $before -> $after)"

# The same authenticated user must actually mine through unrestricted node2.
# Discover its loaded wallet instead of assuming the configurable wallet name.
rpc "$node2_port" \
    '{"jsonrpc":"2.0","id":9,"method":"listwallets","params":[]}'
[ "$RPC_HTTP" != "403" ] || die "node2 unexpectedly denies listwallets"
node2_wallet="$(jq -r '.result[0] // empty' <<<"$RPC_BODY")"
[ -n "$node2_wallet" ] || die "node2 has no loaded wallet; wait for bootstrap before running this test"
wallet_path="/wallet/$(jq -rn --arg wallet "$node2_wallet" '$wallet | @uri')"
rpc "$node2_port" \
    '{"jsonrpc":"2.0","id":10,"method":"getnewaddress","params":[]}' \
    "$wallet_path"
[ "$RPC_HTTP" != "403" ] || die "node2 unexpectedly denies getnewaddress"
mining_address="$(jq -r '.result // empty' <<<"$RPC_BODY")"
[ -n "$mining_address" ] || die "node2 did not return a mining address: $RPC_BODY"

node2_generate="$(jq -nc --arg address "$mining_address" \
    '{jsonrpc:"2.0",id:11,method:"generatetoaddress",params:[1,$address]}')"
rpc "$node2_port" "$node2_generate"
[ "$RPC_HTTP" != "403" ] \
    || die "node2 unexpectedly applies node1's RPC whitelist"
mined_hash="$(jq -r '.result[0] // empty' <<<"$RPC_BODY")"
[ -n "$mined_hash" ] || die "node2 failed to mine through generatetoaddress: $RPC_BODY"

# Node1's RPC restriction does not affect P2P block propagation. Wait until
# node1 knows the exact block node2 just mined (active or stale in a rare race).
node1_received=false
for _ in $(seq 1 30); do
    probe="$(jq -nc --arg hash "$mined_hash" \
        '{jsonrpc:"2.0",id:12,method:"getblockheader",params:[$hash,true]}')"
    rpc "$node1_port" "$probe"
    if [ "$(jq -r '.result.hash // empty' <<<"$RPC_BODY")" = "$mined_hash" ]; then
        node1_received=true
        break
    fi
    sleep 1
done
[ "$node1_received" = "true" ] \
    || die "node1 did not receive node2's mined block $mined_hash over P2P"

echo "Node1 live RPC policy verified (${#denied[@]} denied methods, ${#exceptions[@]} intentional exceptions, atomic batches; node2 mined $mined_hash and node1 received it)"
