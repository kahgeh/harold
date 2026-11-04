# Release Tool

The **release tool** shows, for each product, what its next release holds. It reads the pods' own files and git, and it needs no hand work. It is a [tool](README.md) that only reads: it keeps no database.

The service is `harold.tools.release.v1.ReleaseService`, with one call, `Next`. It trusts local callers (ADR 0002). The proto is `harold-api/proto/tools/release.proto`.

## The command

```sh
harold tool release next                 # every product
harold tool release next harold          # one product
harold tool release next --json          # the same lists, for a program
```

The command takes one optional product name and the flag `--json`. Any other argument gives this refusal: `invalid argument: usage is harold tool release next [product] [--json]`.

The command is read-only:

- It takes no lock. Git runs with `GIT_OPTIONAL_LOCKS=0`, so a read never blocks a build worker.
- It fetches nothing. It reads `origin/<base>` as it stands, so run `git fetch` first if the remote may have moved.
- It writes nothing in the pod folder.

## What the text shows

Harold lists the products of the pod registry in alphabetical order. This is an example:

```text
harold
  merged since the release (v0.73.0)
    timing-tests-local  Run the tests that depend on real time only on this Mac
  in progress
    row-names-plain  Name each control-tower row by its role or its window  (4 commits)
  approved, not started
    linux-satellite  Run Harold on omarchy as a satellite of the Harold on the Mac
  parked
    dependency-audit-guard
  awaiting approval
    session-rename-guard  Refuse an agent's rename of the chief's or a pod's Rex session
  not clear (in progress, no branch)
    harold-agent-metrics
  other commits since v0.73.0: 0

keymap
  no release yet: merged in the last 14 days
    remote-control-key  Open the Harold remote control with one key
  in progress: none
  approved, not started: none

buckwal
  paused: no release yet
```

- Each line is a spec id and its title. The title is the text after `<id>: ` in the first `# ` heading of `specs/<id>.md`.
- The lists "merged since the release", "in progress" and "approved, not started" print `none` when they are empty. The other lists print nothing when they are empty.
- The "in progress" lines also show the commits of the spec's branch over `origin/<base>`, for example `(4 commits)`, `(1 commit)` or `(0 commits)` for a spec just started. When git cannot count them (for example the product has no `origin/<base>`), the line shows no count.
- A paused product prints one line: `paused: last release <tag>` or `paused: no release yet`.
- A product whose git read fails prints `unavailable: <reason>` and no lists.

## The lists and their rules

| List | A spec is in it when |
| --- | --- |
| merged since the release | Its state is `completed` and its merged commit is not in the release tag (one `git rev-list ^<tag> <commits>` call lists the commits in the history of the given commits that the tag does not reach; one `git merge-base --is-ancestor` call per commit answers if that call fails). A completed spec with no merged commit is in the list when its merge time is after the commit time of the tag. |
| in progress | Its state is `in_progress`, it has no parked handoff file and its worktree has a branch. |
| approved, not started | Its state is `approved`, it has no parked handoff file and it was approved on or after the product's first merged file, and its approval names the current text of the spec. An older approval is history. An approval names the current text when `approvals/<id>.md` has no `spec-sha256:` line, or when that line equals the sha-256 of `specs/<id>.md`. |
| parked | The file `parked/<id>-handoff.md` exists, whatever the state of the spec. A parked spec is in no other list. |
| awaiting approval | Its state is `open` and the first line of its spec is `Waiting for your approval.` A spec under the small-fix waiver, or one that starts on the user's exact words, is not listed. A spec whose state is `approved` but whose approval names an older text is also in this list, and its title ends with `(approved at an older text)`. |
| not clear | Its state is `in_progress`, it has no branch and it has no parked handoff file. It may have merged without a merged file, or been given up. |

- **The release** is the newest tag that matches `v[0-9]*` on the base branch of the product (`git describe --tags --abbrev=0 --match 'v[0-9]*' origin/<base>`). Other tags are not releases.
- **No release.** The "merged" list holds each completed spec merged in the last 14 days, under the heading `no release yet: merged in the last 14 days`.
- **Other commits since the release** is one count: the commits on `origin/<base>` after the tag that no commit of a listed merged spec reaches (`git rev-list --count origin/<base> ^<tag> ^<commit> ...`). A spec that merged as several commits adds none. If the count fails, it shows 0. A merged file holds only the tip commit of a spec, and the base branch is linear. So a direct commit made before the newest listed merge is not counted. Only the direct commits after the newest listed merge are counted. The text shows it only when the product has a release.
- Each list is sorted by spec id.

## Where the spec state comes from

The tool never reads the approval, started, merged and withdrawn files with rules of its own. It calls `read_specs` in `harold/src/pod_stats.rs`, the one place that decides a spec's state. The pod metrics use the same function. So the metrics and the next release use one rule for a spec. The tool only sorts the result into lists and adds the facts that the state does not hold: the parked handoff file, the branch, the title, the waiting line and the ancestry of a commit in the tag.

`read_specs` takes an input `Sources.gone`, the worktrees that the pod poller saw go. The tool has no poller, so it passes an empty map. The result differs from the pod metrics only where the poller's record of worktrees seen to go shows a merge.

## The merged_not_released series

The release tool also publishes the work waiting in its "merged since the release" list. This is the one list that only the release tool can build, because only it reads the release tag.

- Every 10 minutes the tool reads the lists of all products.
- For each product it records three gauges with `list` = `merged_not_released`. They are the number of specs (`pod.inventory.items`), the age of the oldest spec in hours (`pod.inventory.oldest_hours`) and the sum of the ages in days (`pod.inventory.item_days`).
- An age counts from the merge.
- The names and units are the constants that `harold/src/pod_stats.rs` exports. Pod stats records the other three lists (`awaiting_approval`, `approved_not_started` and `in_progress`) under the same names. See [Work waiting](../telemetry/README.md#spec-states).
- A paused product, and a product with no git answer, has no series. A product that leaves the registry ends its series. A failed read keeps the last numbers.
- The tool still only reads. It writes nothing in the pod folder and keeps no database. Pod stats does not call the release tool (ADR 0004).

## The paused key

A product is paused when its entry in `products.toml` has `paused = true`. The tool prints one line for it and reads no spec.

## JSON

`--json` prints one object with the key `products`. Each product has the keys of `ProductNext` in the proto:

| Key | Meaning |
| --- | --- |
| `name` | The product name |
| `paused` | `true` for a paused product |
| `release` | The newest release tag; an empty string means no release |
| `merged`, `in_progress`, `approved`, `parked`, `awaiting`, `not_clear` | The lists, each an array of `{ "id", "title" }`. The `in_progress` lines also hold `"commits"` when the count is known, and no `"commits"` key when it is unknown. A known count can be 0. In the gRPC `Line`, `commits_known` says if `commits` is a count. |
| `other_commits` | The count of other commits since the release |
| `unavailable` | A reason when git gave no answer; an empty string otherwise |

## Refusals

The CLI prints one line and exits with an error:

| Line | Cause |
| --- | --- |
| `unknown product <name>` | The registry has no product with this name (`NotFound` `unknown_product`) |
| `the product registry cannot be read` | `[telemetry] pod_registry` is not set, or the file cannot be read (`Unavailable` `registry_unreadable`) |
| `the Harold daemon does not serve releases` | The daemon runs without the release tool (`Unimplemented`) |
| `the Harold daemon refused the call` | The daemon refused with an empty message |
| The server message | Any other refusal prints the message of the server |
| `the Harold daemon is not reachable` | The CLI cannot connect |

## Records the lists need

The lists are only as true as the pods' files. If a merged or withdrawn file is missing, a spec shows in the wrong list. A spec that merged without a merged file shows as "not clear". A spec that was set aside without a withdrawn file stays in "awaiting approval". The lists make these gaps visible.
