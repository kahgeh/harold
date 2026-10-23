#!/bin/sh
# Builds the one OpenTelemetry bundle the web app loads, from web-otel/package-lock.json,
# and writes it to harold/static/web_app/otel-<sdk-trace version>-<8 hex of its sha256>.js.
# Run it only to upgrade, after a fresh audit (docs/references/dependency-audits.md).
# The Rust build and the release never run it. Needs node and npm; installs nothing
# globally and runs no package script.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
source_dir="$root/web-otel"
static_dir="$root/harold/static/web_app"
work=$(mktemp -d "${TMPDIR:-/tmp}/web-otel.XXXXXX")
trap 'rm -rf "$work"' EXIT

cp "$source_dir/package.json" "$source_dir/package-lock.json" "$source_dir/lib.js" "$work/"
cd "$work"
npm ci --ignore-scripts --no-audit --no-fund >/dev/null

./node_modules/.bin/esbuild lib.js --bundle --minify --format=esm --platform=browser \
  --target=safari16 --legal-comments=none --log-level=warning --outfile=body.js

if grep -q -e require-in-the-middle -e import-in-the-middle body.js; then
  echo "the bundle names a Node-only hook" >&2
  exit 1
fi

lock_sha=$(shasum -a 256 "$source_dir/package-lock.json" | cut -d' ' -f1)
sdk_version=$(node -p "require('./node_modules/@opentelemetry/sdk-trace/package.json').version")
version_of() { node -p "require('./node_modules/$1/package.json').version"; }

{
  echo "/* OpenTelemetry JS for the Harold web app, bundled by scripts/build_web_otel.sh."
  echo " * Licence: Apache-2.0 (LICENSES/opentelemetry-js-Apache-2.0.md)."
  for package in api core exporter-trace-otlp-http instrumentation instrumentation-document-load \
    instrumentation-fetch resources sdk-trace sdk-trace-web; do
    echo " * @opentelemetry/$package $(version_of "@opentelemetry/$package") Apache-2.0"
  done
  echo " * @opentelemetry/semantic-conventions $(version_of @opentelemetry/semantic-conventions) Apache-2.0"
  echo " * and the packages these need, as the lockfile pins them."
  echo " * esbuild $(version_of esbuild) MIT (build only)"
  echo " * package-lock.json sha256 $lock_sha"
  echo " */"
  cat body.js
} >bundle.js

hash=$(shasum -a 256 bundle.js | cut -c1-8)
name="otel-$sdk_version-$hash.js"
rm -f "$static_dir"/otel-*.js
cp bundle.js "$static_dir/$name"
echo "$name"
