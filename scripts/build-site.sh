#!/usr/bin/env bash
# Assemble the GitHub Pages site into _site/.
#
# The Pages workflow calls this exact script, so a local preview is
# byte-identical to what gets deployed -- no "works in CI only" surprises.
#
# Usage:
#   ./scripts/build-site.sh              # build into _site/
#   ./scripts/build-site.sh --serve      # build, then serve on :8000
#   ./scripts/build-site.sh --serve 9000 # build, then serve on :9000
#   OUT=/tmp/site ./scripts/build-site.sh

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="${OUT:-$REPO_ROOT/_site}"

serve=false
port=8000
if [[ "${1:-}" == "--serve" ]]; then
  serve=true
  [[ -n "${2:-}" ]] && port="$2"
elif [[ -n "${1:-}" ]]; then
  echo "unknown argument: $1" >&2
  exit 2
fi

rm -rf "$OUT"
mkdir -p "$OUT/dashboard"

# Landing page, walkthrough, and the preview's scripts.
cp "$REPO_ROOT/docs/html/index.html" "$REPO_ROOT/docs/html/walkthrough.html" "$OUT/"
cp "$REPO_ROOT/docs/html/demo-shim.js" "$REPO_ROOT/docs/html/demo-schema.js" "$OUT/dashboard/"

# The preview serves the REAL dashboard, copied verbatim, so it can never
# drift from what the control plane ships.
cp "$REPO_ROOT/crates/control-plane/static/index.html" "$OUT/dashboard/index.html"
cp "$REPO_ROOT/crates/control-plane/static/app.js" "$OUT/dashboard/app.js"
cp "$REPO_ROOT/crates/control-plane/static/styles.css" "$OUT/dashboard/styles.css"

# The control plane serves these from the domain root and substitutes the API
# token into the page at request time. Under Pages the site lives in a
# subdirectory and there is no token, so rewrite both. Every replacement is
# asserted: if the dashboard markup changes shape, fail loudly here rather than
# publish a silently broken preview.
OUT="$OUT" python3 - <<'PY'
import os
from pathlib import Path

page = Path(os.environ["OUT"]) / "dashboard/index.html"
html = page.read_text(encoding="utf-8")

before = html
html = html.replace('"/styles.css"', '"./styles.css"')
html = html.replace('"/app.js"', '"./app.js"')
assert html != before, "expected root-absolute asset paths to rewrite"

assert "__CONTROL_PLANE_TOKEN_JSON__" in html, "token placeholder missing"
html = html.replace("__CONTROL_PLANE_TOKEN_JSON__", '"static-preview-no-backend"')

# The shim must replace window.fetch before app.js runs.
needle = '<script src="./app.js"></script>'
assert needle in html, "app.js script tag not found"
html = html.replace(
    needle,
    '<script src="./demo-schema.js"></script>\n'
    '<script src="./demo-shim.js"></script>\n' + needle,
)

page.write_text(html, encoding="utf-8")
print("dashboard preview prepared")
PY

# Pages would otherwise run the site through Jekyll.
touch "$OUT/.nojekyll"

echo "[build-site] wrote $OUT"
find "$OUT" -type f | sort | sed "s|$OUT|  _site|"

if [[ "$serve" == true ]]; then
  echo
  echo "[build-site] serving http://127.0.0.1:$port/  (Ctrl-C to stop)"
  cd "$OUT" && exec python3 -m http.server "$port"
fi
