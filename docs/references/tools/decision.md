# Decision Tool

The **decision tool** keeps the decision records. A **decision record** is the record of one decision that a role made from evidence, with its check date, status and grades. The coach grades the due records later. The tool is a [tool](README.md) with its own Turso file, `tools/decision/decisions.db`, in Harold's data folder ([ADR 0001](../../adr/0001-tools-are-modules-hosted-by-harold.md)). It trusts local callers ([ADR 0002](../../adr/0002-tool-services-trust-local-callers.md)). The reasons for the design are in [ADR 0039](../../adr/0039-decisions-live-in-a-store-of-their-own-and-their-details-with-what-they-belong-to.md).

The service is `harold.tools.decision.v1.DecisionService`, with the calls `Add`, `List`, `Show`, `Evaluate`, `Recheck` and `Link`. The proto is `harold-api/proto/tools/decision.proto`.

The word "decision" alone means a decision record only in the names of the tool (`harold tool decision`, `decisions.db`, `harold.decisions.*` and its log lines). A work record decision is another thing: see the [work tool](work.md).

## The commands

```sh
harold tool decision add --role pnem --kind slicing --details-owner pnem:harold \
    --accountable pnem --title "Fold the naming fix into agent-restart" \
    --link spec:harold/pod-role-record --check 2026-12-01 \
    --details pod:harold/specs/pod-role-record.decisions.md#d-20261114-02 \
    --asked-at 2026-11-14T03:10Z
harold tool decision list --due
harold tool decision evaluate d-20261101-04 --process good --outcome poor
```

Only the chief's pane runs the write commands (`add`, `evaluate`, `recheck` and `link`). Another pane is refused with `not_chief`. A call with no pane is refused with `no_pane`. The reads need no pane.

| Command | What it does |
| --- | --- |
| `add --role R --kind K --details-owner O --accountable A --title T --check DATE --details PLACE:PATH#ID [--link L] [--asked-at TIME]` | Makes an open record and prints `added <id> (check <date>)`. `TIME` is `YYYY-MM-DDTHH:MMZ` or an RFC 3339 time. |
| `list [--due] [--open] [--all] [--role R] [--kind K] [--details-owner O] [--accountable A] [--json]` | Prints the open and due records, oldest check date first, and then a count line. `--due` and `--open` choose one status. `--all` gives every status. `--json` holds every field. |
| `show ID` | Prints one record with every field, and the full path of its details file. |
| `evaluate ID --process good\|poor --outcome good\|poor` | Sets the grades and the status `evaluated`. |
| `recheck ID --check DATE` | Moves the check date of an open or due record. A due record goes back to open. |
| `link ID LINK` | Sets or replaces the link. |

`list` prints these columns: the id, the date, the role, the kind, the accountable one, the title, the check date and the status.

## The fields

| Field | Meaning |
| --- | --- |
| `id` | `d-YYYYMMDD-NN`. It is the id in the anchor of `details` (see [The id](#the-id)). |
| `decided_on` | The UTC date of the `add`, `YYYY-MM-DD`. |
| `organisation` | The setting `[decision] organisation`. |
| `role` | Who made the decision: `chief`, `pnem`, `pod_coordination`, `build_worker`, `coach` or `user`. |
| `kind` | One of the setting `[decision] kinds`: `slicing`, `model`, `slot`, `design`, `release`, `process` and `other`. A kind that you add to the setting is accepted with no code change. |
| `details_owner` | Who writes the details entry: `chief` or `pnem:<product>`. |
| `accountable` | Whose decision it was: `user`, or the role that made it. |
| `title` | One line of at most 120 characters. |
| `link` | Empty, `work:<name>` or `spec:<product>/<id>`. The tool does not check that the target exists. |
| `check_on` | The check date, `YYYY-MM-DD`. |
| `status` | `open`, `due` or `evaluated`. |
| `process_grade`, `outcome_grade` | `good` or `poor`. Set by `evaluate`. The process grade judges how the decision was made from the evidence known then. The outcome grade judges the result against the expected result. |
| `details` | `<place>:<path>#<id>`. |
| `asked_at`, `added_at`, `evaluated_at` | Times. `asked_at` is the time when the question came to the role. It is optional. |

## The id

The details owner writes the details entry before the chief adds the record. So the id in the anchor of `--details` is the id of the record. `add` accepts the id when all of these are true:

- It has the form `d-YYYYMMDD-NN`, with `NN` of two digits or more.
- Its date is a real date.
- Its date is not after the add date (UTC). An earlier date is allowed, because the details owner can write the entry a day before the chief adds the record.
- No record has this id.

Else `add` refuses with `details_missing`.

## The details

The details live with the thing that the decision record belongs to. The details owner writes each entry under the heading `## <id>: <title>`. Harold only reads the file.

| The decision is about | The file | Details owner | `details` |
| --- | --- | --- | --- |
| One spec | `specs/<id>.decisions.md` next to the spec, in the product's pod folder | `pnem:<product>` | `pod:<product>/specs/<id>.decisions.md#<id>` |
| One product with no spec | `decisions.md` in the product's pod folder | `pnem:<product>` | `pod:<product>/decisions.md#<id>` |
| The pods together, or how the work goes | `decisions.md` in the vault, next to `experiments.md` | `chief` | `vault:decisions.md#<id>` |
| The user personally | A file in the personal repository | `chief` | `personal:<path>#<id>` |

Each place has one source:

| Place | Source |
| --- | --- |
| `pod:<product>` | The `repo` of the product in the pod registry that `telemetry.pod_registry` names, and then `.superpowers/pod/`. |
| `vault:` | The setting `[knowledge] vault`. |
| `personal:` | The setting `[decision] personal_repository`, an absolute path. |

Every place is optional. A Harold with no `knowledge.vault` still takes `pod:` details. The knowledge index reads the vault's `decisions.md` as one more vault note.

## The refusals

`add` and `evaluate` give one reason in the message of the refusal. Both read the details file and check that the heading `## <id>:` exists. `evaluate` reads the details of the stored record, so the entry must still be there. It refuses an evaluated record with `already_evaluated` before it reads the file.

| Reason | When |
| --- | --- |
| `not_chief`, `no_pane` | A write command from a pane that is not the chief's, or from no pane. |
| `bad_role`, `unknown_kind`, `bad_details_owner`, `bad_accountable`, `bad_title`, `bad_link`, `bad_grade` | A field fails its check. |
| `bad_date` | `--check` is not a real date `YYYY-MM-DD`, or `--asked-at` is not a time. |
| `unknown_place` | The place is not `pod`, `vault` or `personal`. |
| `no_pod_registry`, `unknown_product`, `no_vault`, `no_personal_repository` | Harold cannot find the place. `no_pod_registry` also covers a registry that cannot be read. |
| `path_outside_place` | The path is absolute, holds `..`, or reaches outside the place through a symbolic link. |
| `details_unreadable` | Harold cannot read the file. |
| `details_missing` | The file has no heading `## <id>:`, or the id is not allowed (see [The id](#the-id)). |
| `details_owner_mismatch` | A `pod:` place needs `pnem:<product>` of the same product. `vault:` and `personal:` need `chief`. |
| `already_evaluated` | `evaluate` or `recheck` of an evaluated record. |
| `unknown_decision` | No record has this id. |

## The due check

At the start of Harold, and then once a day at 00:05 local time, the tool sets each open record whose `check_on` has come to `due`. The check date is a local date. The tool logs `decision due` for each record that it sets.

## The coach

The coach is a chief subagent, outside Harold. It runs each week, and when the alert `decisions-due` fires. It reads `list --due --json` and each details entry. It gives the chief the grades and the reason. The details owner writes the reason of the evaluation. Then the chief runs `evaluate`. Harold does not start the coach.

## Settings

| Setting | Meaning |
| --- | --- |
| `[decision] organisation` | The name of the organisation. It is an attribute of the metrics. Default `personal-chief`. |
| `[decision] kinds` | The kinds that a record may have. |
| `[decision] personal_repository` | The absolute path of the personal repository. Empty: no `personal:` place. |
| `telemetry.pod_registry` | The pod registry that the `pod:` place reads. |
| `knowledge.vault` | The vault that the `vault:` place reads. |

## Telemetry

- Counters `harold.decisions.added` and `harold.decisions.evaluated`, and the gauge `harold.decisions.open` (`organisation`, `status`). The [telemetry reference](../telemetry/README.md) lists the attributes.
- The feature `decision-tool` on `harold.feature.used`. One use is one `evaluate`.
- Log lines: `decision added`, `decision evaluated` and `decision due` (`info`), and `decision refused` (`warn`, with the field `reason`). No line holds a title or details text.
- The big picture has the card Decisions. The alert `decisions-due` fires when a record is due.
- Harold also logs `decide_s` (the time from `asked_at` to the add) and `to_evaluate_days` (the time from the add to the evaluation).

## Token cost

Harold cannot give the tokens of one decision today: no turn links to a decision. The tokens of the deciding role between `asked_at` and the add are a later piece of work.
