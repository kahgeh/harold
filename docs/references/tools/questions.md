# Question Tool

The question tool is Harold's first [tool](README.md). The chief of pods puts todos in, of two kinds: questions and instructions. The user answers questions and follows instructions from the dash. Harold types the answers back into the chief's pane as one line, a few seconds after they are given, or at once when the user sends every answer with `send` in the dash.

The service is `harold.tools.questions.v1.QuestionService`. It trusts local callers: there is no token. The proto is `harold-api/proto/tools/questions.proto`.

## A question

The chief writes a question as JSON and passes it to `harold tool questions put`:

```json
{
  "id": "Q1",
  "product": "durable-server",
  "spec": "durable-server",
  "sha": "d84bf386a2a5",
  "link": "https://example.com/specs/durable-server.md",
  "intro": "Two choices before the plan.",
  "decisions": [
    {
      "ask": "protocol",
      "options": [
        {"key": "a", "label": "libSQL's Hrana", "impact": "one wire format", "suggested": true},
        {"key": "b", "label": "other", "impact": "", "free": true}
      ]
    }
  ]
}
```

Every field except `intro`, `impact`, `suggested` and `free` is required. The rules:

- `id` is 1 to 32 characters from `A-Z`, `a-z`, `0-9` and `-`.
- `product`, `spec`, `sha` and `link` are not blank. `link` starts with `https://` or `http://`.
- Every text field is under 2,000 characters (1,999 at most).
- There is at least one decision. Each decision has an `ask` and 1 to 9 options.
- Option `key` and `label` are not blank. Keys are unique within a decision. At most one option is `suggested`.
- A `free` option asks for typed text instead of a fixed answer.

A broken rule is refused with the field's name and nothing is stored. The CLI only translates the JSON; the daemon checks the rules.

## Instructions

Instructions are a todo of the other kind: numbered steps the user does by hand. The chief writes them as JSON with `steps` in place of `decisions` and passes them to the same `harold tool questions put`. The question tool holds both in one table, and the API carries instructions in a `Question` message.

```json
{
  "id": "I1",
  "product": "harold",
  "title": "Install Harold 0.15.0 on PasM1Mac",
  "intro": "Do these on PasM1Mac, then tick each one here.",
  "link": "https://example.com/specs/release-install.md",
  "steps": [
    {"text": "Open Terminal"},
    {"text": "Run make deploy in ~/Dev/p/harold/main", "detail": "It asks for your password once."}
  ]
}
```

`id`, `product`, `title` and `steps` are required. `intro`, `link`, `spec` and `sha` are optional. The rules:

- A put holds `decisions` or `steps`, never both. A put with neither is refused as `invalid decisions`; one with both, as `invalid steps`.
- `id` follows the same rule as for a question. `product` and `title` are not blank. `title` is what the dash shows in the instructions' row.
- There are 1 to 20 steps (more than 20 is `invalid steps`). Each step has a `text`, which is not blank, and an optional `detail`.
- Every text field, `text` and `detail` included, is under 2,000 characters. `link`, when present, starts with `https://` or `http://`.
- A question with a `title` is refused as `invalid title`.

A refusal names the field, such as `invalid steps.2.text` or `invalid steps.2.detail`. The ids are the chief's to choose; the tool does not check the first letter, and examples use `Q1` for questions and `I1` for instructions.

Instructions are never a waiting question. They are left out of the `waiting` count in `ListQuestions`, and so they do not hold back questions set aside with `later`.

## The nine RPCs

| RPC | Who | What it does |
| --- | --- | --- |
| `PutQuestion` | chief | Adds a question or instructions. With an existing id it raises the question version and makes the question `waiting` again, with no answer, whatever its state. A waiting or deferred question keeps its place; any other goes last. Returns `id` and `version`. |
| `DismissQuestion` | chief | Withdraws a waiting or deferred question or instructions, with a `reason` the dash shows. If that leaves no question waiting and no answer unsent, the deferred questions return to `waiting`, at the end. |
| `ReorderQuestions` | chief | Puts the listed ids, of questions and instructions, that are waiting or deferred first, in that order, keeping their state. Other ids are ignored, so a race with a dismissal is harmless. |
| `ListQuestions` | dash | Returns the waiting todos, then the deferred ones (flagged `deferred`), the count `waiting`, and `dismissed`: the id and reason of every question dismissed in the last hour. |
| `AnswerQuestion` | dash | Answers a question at the question version the dash saw. |
| `DeferQuestion` | dash | Sets a question aside (`later`). Instructions cannot be set aside: it is refused as `invalid id`. It stays with the other questions and returns to `waiting` later (see [The answers line](#the-answers-line)). |
| `TickStep` | dash | Ticks or unticks one step of waiting instructions (see [Ticking](#ticking)). |
| `SavePicks` | dash, web app | Replaces the saved picks of a waiting or deferred question (see [Saved picks](#saved-picks)). |
| `ShowQuestion` | chief, CLI | Returns a stored question in any state with its question version, state, dismissal reason and saved picks. An id that was never stored is `NotFound` `unknown_question`. |

The `waiting` count in `ListQuestions` is the number of questions listed, deferred ones included. Instructions are listed but not counted. Each listed item carries `ticked`: the step numbers ticked, counting from 1, empty for a question, and `picks`: the saved picks of a question, empty for instructions.

### Ticking

`TickStep` takes `id`, `version`, `step` (counted from 1) and `ticked` (true to tick, false to untick). The ticks are stored with the instructions, in the `ticks` column, so a dash restart, a reconnect or a second dash shows the same ticks. Ticking sends nothing to the chief. When the chief puts the same id again, a step keeps its tick if its text is unchanged.

### Saved picks

`SavePicks` takes `id`, `version` and `picks`, and replaces the question's saved picks. The picks follow the `REVISE` rule below: any picks made so far, in order, with valid keys and `text` for a free pick; an empty list clears them. It is refused as `version_changed`, `not_waiting` (the question is answered, dismissed or unknown) or `invalid picks`, as `AnswerQuestion` is. It changes nothing but the picks: not `updated_at_ms`, the state or the answer, and it does not wake the answers sender. `AnswerQuestion` also writes its picks, so the column always holds the latest. The picks live in the `picks` column, so a dash restart, a reconnect or a second dash shows them, and they stay readable with `ShowQuestion` after the question is answered, sent or dismissed.

When the chief puts the same id again, in any state, a decision takes the saved pick of an old decision whose `ask` and every option field (`key`, `label`, `impact`, `suggested`, `free`) are the same, each old decision used once, in order. The pick's `decision` is renumbered to the new place; other picks are dropped.

### Answers

`AnswerQuestion` takes `id`, `version`, a `kind`, `picks`, a `note` and `send_now`. `send_now` is a flag, false by default: when true, the answer goes to the chief at once (the dash's `send` sets it; see [The answers line](#the-answers-line)). Callers that omit it get false, so they behave as before. A pick has a `decision` (counting from 0), an option `key`, and `text` for a free option.

| Kind | Picks | Note |
| --- | --- | --- |
| `ANSWER` | One per decision, in order, none of them free. | Optional; not sent. |
| `FREE_TEXT` | One per decision, in order, at least one of them free. A free pick must have `text`. | Optional; not sent. |
| `REVISE` | Any picks made so far, in order. | Required: what to change. |
| `REJECT` | Any picks made so far, in order. | Required: why. |
| `DONE` | None. | None. For instructions only. |
| `IGNORE` | None. | None. For instructions only. |

`DONE` tells the chief the user is done with the instructions; `IGNORE` tells the chief the user will not do them. The dash sends both with `send_now` set. `DONE` and `IGNORE` are refused for a question, and `ANSWER`, `FREE_TEXT`, `REVISE` and `REJECT` for instructions, as `invalid kind`. `DONE` and `IGNORE` with picks or a note are refused as `invalid picks` or `invalid note`. `UNSPECIFIED` is refused. A free option does not end the walk through the decisions: its typed text becomes that decision's pick, and the dash goes on to the next decision. The answer is `FREE_TEXT` when any pick is free.

The note is sent only with `REVISE` and `REJECT`. For `ANSWER` and `FREE_TEXT` it is not sent; the typed text of a free pick travels as that pick.

### Refusals

| Status | Message | When |
| --- | --- | --- |
| `InvalidArgument` | `invalid id` | `DeferQuestion` names instructions, which `later` does not apply to. |
| `InvalidArgument` | `invalid <field>` | A rule is broken. The field is named, such as `invalid link`, `invalid decisions.1.options.2.key`, `invalid picks`, `invalid note`, `invalid kind`, `invalid steps` or `invalid step`. Nothing is stored. |
| `FailedPrecondition` | `version_changed` | The question or instructions were put again since the dash read them. |
| `FailedPrecondition` | `not_waiting` | The question or instructions are dismissed, already answered, or unknown. |
| `InvalidArgument` | `invalid step` | `TickStep` names a step number below 1 or above the number of steps, which includes any step of a question. `TickStep` checks `not_waiting` and `version_changed` first. |
| `NotFound` | `unknown_question` | `ShowQuestion` names an id that was never stored. |
| `Unavailable` | `the question database is unavailable` | A database write failed. |

## The CLI

The chief uses `harold tool questions`. Each command prints one line, or the refusal's message.

```sh
harold tool questions put < question.json        # stored Q1 v1
harold tool questions list                       # one line per question, then "<n> waiting"
harold tool questions order Q3 Q1              # ordered 2
harold tool questions put < instructions.json     # stored I1 v1
harold tool questions dismiss Q1 --reason "spec withdrawn"   # dismissed Q1
harold tool questions show Q1                    # the question and each decision's pick
```

`put` accepts the JSON for a question or for instructions. `order` and `dismiss` take ids of either. `list` prints `<id> v<version> <product> <spec> picks <picked>/<decisions>`, with ` (deferred)` after a deferred one, and `<id> v<version> <product> steps <ticked>/<n>` for instructions, such as `I1 v1 harold steps 2/4`. `show` prints a head line `<id> v<version> <product> <spec> <state>`, then one line per decision: `  <n> <ask>: <key> <label>`, `  <n> <ask>: <key> "<text>"` for a free pick, or `  <n> <ask>: no pick`. Any other use prints `invalid argument: usage is harold tool questions put|dismiss|order|list|show`. If the daemon is not running the CLI prints `the Harold daemon is not reachable`, and after 3 seconds without an answer, `the Harold daemon did not answer within 3 seconds`.

The user answers in the dash, not through the CLI. See the [dash README](../../../tmx-agent-dash/README.md#todos).

## States

A question, or a set of instructions, is in one state:

- `waiting`: it needs an answer.
- `deferred`: set aside with `DeferQuestion`. It can still be answered. It returns to `waiting`, at the end, after a line is typed with no question waiting, at a `later` that leaves no question waiting and no answer unsent, and when the chief dismisses the last waiting question and no answer is unsent.
- `answered`: it has an answer that is not yet in a batch.
- `sent`: its answer was typed to the chief.
- `dismissed`: the chief withdrew it.

`PutQuestion` on a dismissed or sent id makes it `waiting` again at the next question version.

## The answers line

A batch starts when at least one answer is unsent and 3 seconds (the settle) have passed since the newest unsent answer was given, or at once when any unsent answer has `send_now` set. Questions still `waiting` do not hold it back. The line holds every unsent answer, whether or not `send_now` is set, so an answer still in its settle goes in it and its settle ends. Answers given within 3 seconds of each other go in one line. A batch also needs the answers to be unchanged since they were read: if a question is put again between reading and starting, nothing starts and the sender looks again. A question put again loses its `send_now` flag with its answer.

The settle is measured from the answer's own `updated_at_ms` in the store. It does not restart on other writes, such as a new question or a `later`, or on the 5-second retry. After a restart of Harold, unsent answers wait at most one settle, and one with `send_now` set goes at once. An answer given while a line is being typed is not in that line; it stays unsent and goes in the next line, once.

The line is typed into the chief's pane with Enter:

```text
ANSWERS 3 | Q1 durable-server sha d84bf386a2a5: protocol libSQL's Hrana; build yes | Q2 voice-x: revise "make the button bigger" | Q3 old-y: reject "not needed" | later: Q4
```

- `ANSWERS <n>` counts answers, not the ids set aside.
- An `answer` or `free_text` part is `<id> <spec> sha <sha>: <pick>; <pick>`. A pick is `<ask> <label>`. A free option's pick is `<ask> "<typed text>"`, for example `Q4 net-z sha 3333: Which? "use HTTP/3"`.
- A `revise` or `reject` part is `<id> <spec>: revise "<note>"` or `<id> <spec>: reject "<note>"`. It has no `sha`.
- ` | later: <id>, <id>` is added only when some question is set aside. It lists the questions still set aside when the line is built, so an early line can repeat an id that stays set aside.
- A part for instructions is `<id> <product>: done`, `<id> <product>: done, <ticked> of <n> ticked` when some steps are unticked, or `<id> <product>: ignored`. It has no `sha`, and counts in `ANSWERS <n>`. For example, `I1 harold: done, 3 of 4 ticked`.
- The line does not say which answer had `send_now` set.

Answers come in the order of the questions.

### Cleaning

Every field is cleaned before it goes into the line, so a question cannot inject keystrokes into the chief's pane:

- `\n` and `\r\n` become a space.
- A lone `\r`, tabs and every other control character are removed.
- An escape sequence is removed: `ESC`, and for `ESC [` everything up to and including the final byte in `0x40` to `0x7E`.

A `|` in a field is not changed, so a note that holds `|` reads like a separator. The chief reads the line by its `ANSWERS <n>` count and the `<id> <spec>` heads.

### Timing and restart

Each round, the sender looks up the chief's pane and makes one try at typing the batch: the pane must be idle with an empty input box. It does not wait for a busy pane inside the try. If there is no chief, the pane is busy or gone, or the database fails, it tries again 5 seconds later, without limit. It logs `answers batch waiting` when a spell of waiting starts, with its `reason`, and again each time the `reason` changes, for example `store_failed` when a release fails after `pane_busy`. The spell ends when a batch is typed or nothing is left to send.

The batch is stored with its text before the try. If nothing was typed (no chief, a busy pane, no pane), a batch built since Harold started is released: its row is deleted and its answers are unsent again, in one transaction. The next round builds it again from what is there then, so an answer given while a batch waits joins it, and the chief gets one line, not two. An answer's `send_now` flag stays set through the retry. If the release fails, it is tried again before anything else. A panic in the typing job counts as a busy pane, so its batch is released too.

While nothing is to be sent, the sender wakes on each write, and also looks again every 30 seconds, in case a write was stored by an RPC that was cancelled before it woke the sender.

Once a batch is typed, it is marked typed:

- Its answers become `sent`.
- Every question set aside returns to `waiting`, at the end, if no question is `waiting`. Otherwise they stay set aside, so `later` keeps meaning "after everything else".

The same rule applies when the chief dismisses a question: if no question is `waiting` and no answer is unsent, the questions set aside return at once.

If Enter fails after the text went in, the batch counts as typed and is never typed again, since the text is already in the box.

If Harold stops while a batch is being typed, or between typing and marking, the batch may have reached the pane, so it is still open at the next start. That leftover batch is kept: it waits for the chief and an idle pane like any batch, but it is never released, and it is typed again once, before any new batch, even if a question waits. Such a line is the only case that can repeat. A batch whose try typed nothing was released, so it is never typed twice. If Harold crashes during a release, the batch is still open at the next start, so it is typed as a leftover batch, even if a question waits.

## Log lines

Ten fixed lines, with only the fields `tool`, `question_id`, `version`, `reason`, `count` and `after`. No question or answer text is ever logged.

| Line | Fields | Notes |
| --- | --- | --- |
| `question stored` | `tool`, `question_id`, `version` | |
| `question dismissed` | `tool`, `question_id`, `version` | |
| `question answered` | `tool`, `question_id`, `version` | |
| `step ticked` | `tool`, `question_id`, `version`, `reason` | `reason` is `ticked` or `unticked`. No step text is logged. |
| `picks saved` | `tool`, `question_id`, `version` | Info. No pick, key or text is logged. |
| `picks not saved` | `tool`, `question_id`, `version`, `reason` | A warning; `reason` is `store_failed`. |
| `answers batch typed` | `tool`, `reason` | `reason` is `typed`, or `enter_failed` when Enter failed after the text went in (a warning). |
| `answers batch waiting` | `tool`, `reason` | `reason` is `no_chief`, `pane_busy`, `no_pane` or `store_failed`. |
| `set-aside questions returned` | `tool`, `count`, `after` | Info, written once per return and only when `count` is above 0. `after` is `answers_line`, `later` or `dismiss`. No question id or text is logged. It is written by the store as the questions move, so it comes just before the `question dismissed` or `answers batch typed` line that caused it, and in the rare case that the write then fails it may stand for a return that did not happen. |
| `tool failed to start` | `tool`, `reason` | `reason` is `store_open_failed`. |

## Data folder and rollback

The data folder is `<parent of the store path>/tools/questions/`. It holds `questions.db`, a Turso database with two tables: `questions` (the question or instructions, their state and answer, the ticked step numbers, the saved picks, and the dismissal `reason`) and `batches` (each line's text and whether it was typed).

Migration `002_answer_send_now.sql` adds `send_now INTEGER NOT NULL DEFAULT 0` to `questions`; existing rows get 0. A Harold that has the questions tool but not migration 002 refuses the migrated `questions.db`, as it does for any migration, so rolling back to it means moving `questions.db` aside.

Migration `003_step_ticks.sql` adds `ticks TEXT NOT NULL DEFAULT ''` to `questions`: the ticked step numbers. The instructions themselves go in the existing `body` column. An older Harold refuses the migrated `questions.db`; roll back by moving `questions.db` aside.

Migration `004_saved_picks.sql` adds `picks TEXT NOT NULL DEFAULT ''` to `questions`: the latest picks as JSON, one entry per picked decision (`decision`, `key`, `text`). An older Harold refuses the migrated `questions.db`; roll back by moving `questions.db` aside, as for 002 and 003.

The core store is untouched. To roll back:

- Run an older Harold. It ignores `tools/questions/`.
- Move `questions.db` aside (or delete it) to empty the question tool. Harold makes a fresh one at the next start.
