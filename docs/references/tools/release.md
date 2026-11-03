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
- The "in progress" lines also show the commits of the spec's branch over `origin/<base>`.
- A paused product prints one line: `paused: last release <tag>` or `paused: no release yet`.
- A product whose git read fails prints `unavailable: <reason>` and no lists.

## The lists and their rules

| List | A spec is in it when |
| --- | --- |
| merged since the release | Its state is `completed` and its merged commit is not in the release tag (`git merge-base --is-ancestor`). A completed spec with no merged commit is in the list when its merge time is after the commit time of the tag. |
| in progress | Its state is `in_progress`, it has no parked handoff file and its worktree has a branch. |
| approved, not started | Its state is `approved`, it has no parked handoff file and it was approved on or after the product's first merged file. An older approval is history. |
| parked | The file `parked/<id>-handoff.md` exists, whatever the state of the spec. A parked spec is in no other list. |
| awaiting approval | Its state is `open` and the first line of its spec is `Waiting for your approval.` A spec under the small-fix waiver, or one that starts on the user's exact words, is not listed. |
| not clear | Its state is `in_progress`, it has no branch and it has no parked handoff file. It may have merged without a merged file, or been given up. |

- **The release** is the newest tag that matches `v[0-9]*` on the base branch of the product (`git describe --tags --abbrev=0 --match 'v[0-9]*' origin/<base>`). Other tags are not releases.
- **No release.** The "merged" list holds each completed spec merged in the last 14 days, under the heading `no release yet: merged in the last 14 days`.
- **Other commits since the release** is one count: the commits on `origin/<base>` after the tag that no listed merged spec holds. A direct fix does not stay hidden. The text shows it only when the product has a release.
- Each list is sorted by spec id.

## Where the spec state comes from

The tool never reads the approval, started, merged and withdrawn files with rules of its own. It calls `read_specs` in `harold/src/pod_stats.rs`, the one place that decides a spec's state. The pod metrics use the same function. So the metrics and the next release cannot disagree about a spec. The tool only sorts the result into lists and adds two facts that the state does not hold: the parked handoff file and the branch.

`read_specs` takes an input `Sources.gone`, the worktrees that the pod poller saw go. The tool has no poller, so it passes an empty map. The result differs from the pod metrics only where the base branch keeps no reflog.

## The paused key

A product is paused when its entry in `products.toml` has `paused = true`. The tool prints one line for it and reads no spec.

## JSON

`--json` prints one object with the key `products`. Each product has the keys of `ProductNext` in the proto:

| Key | Meaning |
| --- | --- |
| `name` | The product name |
| `paused` | `true` for a paused product |
| `release` | The newest release tag; an empty string means no release |
| `merged`, `in_progress`, `approved`, `parked`, `awaiting`, `not_clear` | The lists, each an array of `{ "id", "title" }`. The `in_progress` lines also hold `"commits"`. |
| `other_commits` | The count of other commits since the release |
| `unavailable` | A reason when git gave no answer; an empty string otherwise |

## Refusals

The CLI prints one line and exits with an error:

| Line | Cause |
| --- | --- |
| `unknown product <name>` | The registry has no product with this name (`NotFound` `unknown_product`) |
| `the product registry cannot be read` | `[telemetry] pod_registry` is not set, or the file cannot be read (`Unavailable` `registry_unreadable`) |
| `the Harold daemon does not serve releases` | The daemon runs without the release tool (`Unimplemented`) |
| `the Harold daemon is not reachable` | The CLI cannot connect |

## Records the lists need

The lists are only as true as the pods' files. If a merged or withdrawn file is missing, a spec shows in the wrong list. A spec that merged without a merged file shows as "not clear". A spec that was set aside without a withdrawn file stays in "awaiting approval". The lists make these gaps visible.
