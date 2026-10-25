#!/bin/sh
# Builds the one Mermaid bundle the spec pages load, from web-mermaid/package-lock.json,
# and writes it to harold/static/web_app/mermaid-<mermaid version>-<8 hex of its sha256>.js.
# Run it only to upgrade, after a fresh audit (docs/references/dependency-audits.md).
# The bundle leaves out elkjs (EPL-2.0): esbuild puts web-mermaid/empty-elk.js in its place.
# The Rust build and the release never run it. Needs node and npm; installs nothing
# globally and runs no package script.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
source_dir="$root/web-mermaid"
static_dir="$root/harold/static/web_app"
work=$(mktemp -d "${TMPDIR:-/tmp}/web-mermaid.XXXXXX")
trap 'rm -rf "$work"' EXIT

cp "$source_dir/package.json" "$source_dir/package-lock.json" "$source_dir/lib.js" "$source_dir/empty-elk.js" "$work/"
cd "$work"
npm ci --ignore-scripts --no-audit --no-fund >/dev/null

./node_modules/.bin/esbuild lib.js --bundle --minify --format=esm --platform=browser \
  --target=safari16 --legal-comments=none --log-level=warning \
  --alias:elkjs/lib/elk.bundled.js=./empty-elk.js --outfile=body.js

if grep -q -e child_process -e node:fs body.js; then
  echo "the bundle names a Node-only module" >&2
  exit 1
fi

if grep -q org.eclipse.elk body.js; then
  echo "the bundle holds elkjs" >&2
  exit 1
fi

lock_sha=$(shasum -a 256 "$source_dir/package-lock.json" | cut -d' ' -f1)
version_of() { node -p "require('./node_modules/$1/package.json').version"; }
mermaid_version=$(version_of mermaid)

{
  echo "/* Mermaid for the Harold spec pages, bundled by scripts/build_web_mermaid.sh."
  echo " * Licences: LICENSES/mermaid-bundle.md."
  echo " * mermaid $mermaid_version MIT"
  echo " * dompurify $(version_of dompurify) Apache-2.0 (dual MPL-2.0 or Apache-2.0)"
  echo " * and the packages these need, as the lockfile pins them."
  echo " * esbuild $(version_of esbuild) MIT (build only)"
  echo " * package-lock.json sha256 $lock_sha"
  echo " */"
  cat body.js
} >bundle.js

hash=$(shasum -a 256 bundle.js | cut -c1-8)
name="mermaid-$mermaid_version-$hash.js"
rm -f "$static_dir"/mermaid-*.js
cp bundle.js "$static_dir/$name"
echo "$name"
