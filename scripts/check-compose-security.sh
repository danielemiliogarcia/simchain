#!/usr/bin/env bash
# Assert the final control-plane and namespace-agent trust boundary against the
# fully rendered Compose model, not merely the source YAML spelling.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
config="$(mktemp)"
trap 'rm -f "$config"' EXIT

cd "$repo_root"
"$repo_root/scripts/check-node1-rpc-policy.sh"
docker compose config --format json >"$config"

jq -e '
  [
    .services[]
    | (.volumes // [])[]
    | select(
        (.source // "") == "/var/run/docker.sock"
        or (.target // "") == "/var/run/docker.sock"
      )
  ] | length == 0
' "$config" >/dev/null

jq -e '
  .services["btc-simnet-control-plane"] as $control
  | $control != null
  and (($control.profiles // []) | length == 0)
  and $control.read_only == true
  and (($control.cap_drop // []) == ["ALL"])
  and (($control.security_opt // []) | index("no-new-privileges:true") != null)
  and (($control.volumes // []) | length == 1)
  and $control.volumes[0].type == "volume"
  and $control.volumes[0].target == "/var/lib/simchain-control"
  and $control.volumes[0].source == "control-state"
  and ((($control.networks // {}) | keys) == ["btc-simnet-control"])
  and (($control.ports // []) | all(.host_ip == "127.0.0.1"))
  and (
    ($control.environment // {})
    | keys
    | map(select(
        . == "DOCKER_HOST"
        or . == "SIMCHAIN_REPO_ROOT"
        or . == "SIMCHAIN_ENV_FILE"
        or startswith("COMPOSE_")
      ))
    | length == 0
  )
  and (.services["btc-simnet-scenario"] == null)
' "$config" >/dev/null

for node in node1 node2 node3; do
  service="btc-simnet-network-agent-$node"
  jq -e --arg service "$service" --arg node "$node" '
    .services[$service] as $agent
    | $agent != null
    and $agent.network_mode == ("service:btc-simnet-" + $node)
    and (($agent.ports // []) | length == 0)
    and (($agent.cap_drop // []) == ["ALL"])
    and (($agent.cap_add // []) == ["NET_ADMIN"])
    and (($agent.security_opt // []) | index("no-new-privileges:true") != null)
  ' "$config" >/dev/null
done

jq -e '
  . as $root
  | .services["btc-simnet-node1"] as $node1
  | $node1.environment.FILTER_NODE1_RPC as $filter
  | ("node1-rpc-" + $filter) as $policy
  | $node1.entrypoint == null
  and (["true", "false"] | index($filter) != null)
  and (($node1.configs // []) | length == 1)
  and $node1.configs[0].source == $policy
  and $node1.configs[0].target == "/etc/bitcoin/node1-rpc.conf"
  and (($node1.volumes // []) | all(.type != "bind"))
  and $node1.command[0] == "-conf=/etc/bitcoin/node1-rpc.conf"
  and $node1.command[1] == "-printtoconsole"
  and ($node1.command | all(contains("/bin/bash") | not))
  and ($root.configs[$policy] != null)
  and (
    if $filter == "true" then
      ($root.configs[$policy].content | contains("rpcwhitelistdefault=0"))
      and ($root.configs[$policy].content
           | contains("rpcwhitelist=" + $node1.environment.BTC_RPC_USER + ":"))
      and ($root.configs[$policy].content | contains("rpcauth=simchain-internal:"))
    else
      ($root.configs[$policy].content | contains("rpcwhitelist") | not)
      and ($root.configs[$policy].content | contains("rpcauth=simchain-internal:"))
    end
  )
  and ($root.services["btc-simnet-control-plane"].environment.NODE1_INTERNAL_RPC_USER == "simchain-internal")
  and ($root.services["btc-simnet-control-plane"].environment.NODE1_INTERNAL_RPC_PASS == "simchain-internal-rpc-password")
  and (
    ["btc-simnet-node2", "btc-simnet-node3"]
    | all(. as $service
        | (($root.services[$service].configs // []) | length == 0)
        and ($root.services[$service].entrypoint == null)
        and ($root.services[$service].environment.FILTER_NODE1_RPC == null)
        and (($root.services[$service].command // [])
             | all((contains("rpcwhitelist") or contains("node1-rpc.conf")) | not))
      )
  )
' "$config" >/dev/null

echo "Compose security boundary verified"
