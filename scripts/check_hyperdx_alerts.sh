#!/usr/bin/env bash
# Fails when HyperDX lacks an alert that docs/references/telemetry/hyperdx/alert-*.json defines, or holds one that differs
# from its file. Reads HYPERDX_URL and HYPERDX_API_KEY from the environment, as the README's commands do.
# Exit 0: every alert matches. Exit 1: one line per problem. Exit 2: HyperDX could not be checked.
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

: "${HYPERDX_URL:?check_hyperdx_alerts: set HYPERDX_URL}"
: "${HYPERDX_API_KEY:?check_hyperdx_alerts: set HYPERDX_API_KEY}"

dir=docs/references/telemetry/hyperdx

# The key goes to curl on stdin, so it never shows in a process list.
fetch() {
    printf 'header = "Authorization: Bearer %s"\n' "$HYPERDX_API_KEY" | curl -sS -m 20 -K - "$HYPERDX_URL/api/v2/$1"
}

cannot_check() {
    echo "check_hyperdx_alerts: cannot check, $1" >&2
    exit 2
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
      | if ($found | length) == 0 then "alert \($want.name): missing in HyperDX"
        elif ($found | length) > 1 then "alert \($want.name): HyperDX has \($found | length) of that name"
        else
          $found[0] as $alert
          | (
              ["threshold", "thresholdType", "interval", "numConsecutiveWindows"][] as $field
              | select(($alert[$field] // null) != $want[$field])
              | "alert \($want.name): \($field) is \($alert[$field] // null | show) in HyperDX, \($want[$field] | show) in the file"
            ),
            (
              if ($hook | length) == 0 then "webhook harold-alerts-\($want.name): missing in HyperDX"
              elif ($hook | length) > 1 then "webhook harold-alerts-\($want.name): HyperDX has \($hook | length) of that name"
              elif $alert.channel.webhookId != $hook[0].id then "alert \($want.name): does not point at webhook harold-alerts-\($want.name)"
              else empty end
            )
        end
    ),
    (
      $have[] | select((.tags // []) | index("harold")) | select(.name as $n | $names | index($n) | not)
      | "alert \(.name): tagged harold in HyperDX, has no file"
    )
')

if [ -n "$problems" ]; then
    echo "$problems"
    echo "check_hyperdx_alerts: $(echo "$problems" | wc -l | tr -d ' ') problems"
    exit 1
fi
echo "check_hyperdx_alerts: ok, ${#files[@]} alerts"
