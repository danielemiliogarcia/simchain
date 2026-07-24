#!/usr/bin/env bash
# Install runtime prerequisites and inject host-side diagnostic tools into one
# running Bitcoin node container, or into every running Simchain Bitcoin node.

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
chainwatch_source="$script_dir/chainwatch.sh"
chainwatch_target="/usr/local/bin/chainwatch"

usage() {
    cat <<EOF
Usage:
  $0 CONTAINER
  $0 --all-containers

Install tool prerequisites and copy the Simchain diagnostic tools into running
Bitcoin node containers. Currently this installs curl and injects chainwatch as
$chainwatch_target, which is available on the container's PATH.

Arguments:
  CONTAINER          A running Bitcoin node container, for example
                     btc-simnet-node3.
  --all-containers   Inject into every running btc-simnet-nodeN container.
  -h, --help         Show this help.

Examples:
  $0 btc-simnet-node3
  $0 --all-containers
EOF
}

die() {
    echo "[ERROR] $*" >&2
    exit 1
}

require_host_tools() {
    command -v docker >/dev/null 2>&1 || die "docker is required but not found"
    [ -f "$chainwatch_source" ] || die "tool not found: $chainwatch_source"
}

require_running_container() {
    local container="$1" running
    running="$(docker inspect --format '{{.State.Running}}' "$container" 2>/dev/null)" || \
        die "container does not exist: $container"
    [ "$running" = "true" ] || die "container is not running: $container"
}

install_curl() {
    local container="$1"

    if docker exec "$container" sh -c 'command -v curl >/dev/null 2>&1'; then
        echo "[OK] $container: curl is already installed"
        return
    fi

    echo "[INFO] $container: installing curl"
    docker exec -u root -e DEBIAN_FRONTEND=noninteractive "$container" sh -c '
        if command -v apt-get >/dev/null 2>&1; then
            apt-get update -qq
            apt-get install -y -qq curl
        else
            echo "[ERROR] curl is missing and apt-get is unavailable" >&2
            exit 1
        fi
    '
}

inject_container() {
    local container="$1"

    require_running_container "$container"
    install_curl "$container"

    echo "[INFO] $container: copying chainwatch.sh to $chainwatch_target"
    docker cp "$chainwatch_source" "$container:$chainwatch_target"
    docker exec -u root "$container" chmod 0755 "$chainwatch_target"
    docker exec "$container" bash -n "$chainwatch_target"
    echo "[OK] $container: tools injected"
}

all_node_containers() {
    docker ps \
        --filter 'name=^/btc-simnet-node[0-9]+$' \
        --format '{{.Names}}' \
        | sort
}

main() {
    local mode container
    local -a containers=()

    [ "$#" -gt 0 ] || { usage >&2; exit 1; }

    case "$1" in
        -h|--help)
            [ "$#" -eq 1 ] || die "$1 does not accept additional arguments"
            usage
            return
            ;;
        --all-containers)
            [ "$#" -eq 1 ] || die "--all-containers does not accept a container name"
            mode="all"
            ;;
        --*)
            die "unknown option: $1"
            ;;
        *)
            [ "$#" -eq 1 ] || die "provide exactly one container name"
            mode="single"
            container="$1"
            ;;
    esac

    require_host_tools

    if [ "$mode" = "single" ]; then
        inject_container "$container"
        return
    fi

    mapfile -t containers < <(all_node_containers)
    [ "${#containers[@]}" -gt 0 ] || \
        die "no running btc-simnet-nodeN containers were found"

    for container in "${containers[@]}"; do
        inject_container "$container"
    done
}

main "$@"
