#!/usr/bin/env bash
# Build every Rust-tool image target and inspect the final control-plane rootfs.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
tag_prefix="${SIMCHAIN_IMAGE_TEST_PREFIX:-simchain-contract}"
targets=(
  mining-controller
  spammer
  reorg
  scenario-engine
  control-plane
  network-agent
  simchainctl
)

cd "$repo_root"
for target in "${targets[@]}"; do
  docker build \
    --file docker/tools.Dockerfile \
    --target "$target" \
    --tag "$tag_prefix-$target:latest" \
    .
done

control_image="$tag_prefix-control-plane:latest"
cid="$(docker create "$control_image")"
rootfs_listing="$(mktemp)"
cleanup() {
  docker rm "$cid" >/dev/null
  rm -f "$rootfs_listing"
}
trap cleanup EXIT

docker export "$cid" | tar -tf - >"$rootfs_listing"
grep_status=0
forbidden="$(grep -E '(^|/)(docker|docker-compose|compose|sh|bash|dash|apt|apt-get|dpkg)$' "$rootfs_listing")" \
  || grep_status=$?
if ((grep_status > 1)); then
  echo "could not inspect the control-plane rootfs" >&2
  exit "$grep_status"
fi
if [[ -n "$forbidden" ]]; then
  echo "forbidden control-plane executables found:" >&2
  echo "$forbidden" >&2
  exit 1
fi

entrypoint="$(docker image inspect "$control_image" --format '{{json .Config.Entrypoint}}')"
healthcheck="$(docker image inspect "$control_image" --format '{{json .Config.Healthcheck.Test}}')"
[[ "$entrypoint" == '["/simchain-control-plane"]' ]]
[[ "$healthcheck" == '["CMD","/simchainctl","--url","http://127.0.0.1:8080","status"]' ]]

echo "Docker image targets built; control-plane rootfs boundary verified"
