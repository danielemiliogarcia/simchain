#!/usr/bin/env bash
# Assert the final control-plane and namespace-agent trust boundary against the
# fully rendered Compose model, not merely the source YAML spelling.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
config="$(mktemp)"
trap 'rm -f "$config"' EXIT

cd "$repo_root"
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
  and $control.volumes[0].type == "bind"
  and $control.volumes[0].target == "/var/lib/simchain-control"
  and ($control.volumes[0].source | endswith("/.simchain-control"))
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

echo "Compose security boundary verified"
