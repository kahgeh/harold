#!/usr/bin/env bash
# Fails when HyperDX lacks an alert that docs/references/telemetry/hyperdx/alert-*.json defines, or holds one that differs
# from its file. Reads HYPERDX_URL from the environment. Reads the HyperDX access key, on one line, from the file named by
# HYPERDX_API_KEY_FILE (default ~/.config/harold/hyperdx-access-key). The file must be a non-empty regular file that
# only its owner can read (mode 0600 or 0400) and that lies outside every git checkout, as for the knowledge tool's key files.
# The mode check uses BSD stat, so it works on macOS only. A GIT_DIR in the environment makes the git-checkout rule refuse every key file.
# Exit 0: every alert matches. Exit 1: one line per problem. Exit 2: HyperDX could not be checked.
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

cannot_check() {
    echo "check_hyperdx_alerts: cannot check, $1" >&2
    exit 2
}

[ -n "${HYPERDX_URL:-}" ] || cannot_check "set HYPERDX_URL"

key_file=${HYPERDX_API_KEY_FILE:-$HOME/.config/harold/hyperdx-access-key}
[ -e "$key_file" ] || cannot_check "$key_file: missing"
[ -f "$key_file" ] || cannot_check "$key_file: not a regular file"
[ -s "$key_file" ] || cannot_check "$key_file: empty"
mode=$(stat -L -f %Lp "$key_file" 2>/dev/null || stat -L -c %a "$key_file") || cannot_check "$key_file: cannot read its mode"
[ $((8#$mode & 8#077)) -eq 0 ] || cannot_check "$key_file: readable by group or others (mode $mode), run chmod 600"
git -C "$(dirname "$key_file")" rev-parse >/dev/null 2>&1 && cannot_check "$key_file: inside a git checkout"
IFS= read -r key <"$key_file" || [ -n "$key" ] || cannot_check "$key_file: cannot read it"
[ -n "$key" ] || cannot_check "$key_file: empty"

# curl's config syntax needs \ and " in a quoted value escaped.
key=${key//\\/\\\\}
key=${key//\"/\\\"}

dir=docs/references/telemetry/hyperdx

# The key goes to curl on stdin, so it never shows in a process list.
fetch() {
    printf 'header = "Authorization: Bearer %s"\n' "$key" | curl -sS -m 20 -K - "$HYPERDX_URL/api/v2/$1"
}

alerts=$(fetch alerts) || cannot_check "no answer from $HYPERDX_URL"
webhooks=$(fetch webhooks) || cannot_check "no answer from $HYPERDX_URL"
for list in "$alerts" "$webhooks"; do
    printf '%s' "$list" | jq -e '.data | type == "array"' >/dev/null 2>&1 || cannot_check "$HYPERDX_URL did not answer with a list"
done

files=("$dir"/alert-*.json)
[ -e "${files[0]}" ] || cannot_check "no alert files in $dir"

# What the files say each alert must be. numConsecutiveWindows is null for a file without it, and HyperDX returns null
# for an alert created without it.
expected=$(jq -s -c 'map({name, threshold, thresholdType, interval, numConsecutiveWindows: (.numConsecutiveWindows // null)})' "${files[@]}")

problems=$(jq -n -r --argjson expected "$expected" --argjson alerts "$alerts" --argjson webhooks "$webhooks" '
  def show: if . == null then "none" else tostring end;
  ($alerts.data) as $have
  | ($webhooks.data) as $hooks
  | ($expected | map(.name)) as $names
  | (
      $expected[] as $want
      | ($have | map(select(.name == $want.name))) as $found
      | ($hooks | map(select(.name == "harold-alerts-\($want.name)"))) as $hook
      | (
          if ($found | length) == 0 then "alert \($want.name): missing in HyperDX"
          elif ($found | length) > 1 then "alert \($want.name): HyperDX has \($found | length) of that name"
          else
            $found[0] as $alert
            | ["threshold", "thresholdType", "interval", "numConsecutiveWindows"][] as $field
            | select(($alert[$field] // null) != $want[$field])
            | "alert \($want.name): \($field) is \($alert[$field] // null | show) in HyperDX, \($want[$field] | show) in the file"
          end
        ),
        (
          if ($hook | length) == 0 then "webhook harold-alerts-\($want.name): missing in HyperDX"
          elif ($hook | length) > 1 then "webhook harold-alerts-\($want.name): HyperDX has \($hook | length) of that name"
          elif ($found | length) == 1 and $found[0].channel.webhookId != $hook[0].id then "alert \($want.name): does not point at webhook harold-alerts-\($want.name)"
          else empty end
        )
    ),
    (
      $have[] | select((.tags // []) | index("harold")) | select(.name as $n | $names | index($n) | not)
      | "alert \(.name): tagged harold in HyperDX, has no file"
    )
') || cannot_check "could not compare HyperDX with the files"

if [ -n "$problems" ]; then
    echo "$problems"
    echo "check_hyperdx_alerts: $(echo "$problems" | wc -l | tr -d ' ') problems"
    exit 1
fi
echo "check_hyperdx_alerts: ok, ${#files[@]} alerts"
