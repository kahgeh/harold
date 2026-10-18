# Question Queue

The question queue is Harold's first [tool](README.md). The chief of pods puts questions in. The user answers them from the dash. When every question has an answer, Harold types the answers back into the chief's pane as one line.

The service is `harold.tools.questions.v1.QuestionService`. It trusts local callers: there is no token. The proto is `harold-api/proto/tools/questions.proto`.

## A question

The chief writes a question as JSON and passes it to `harold tool questions put`:

```json
{
  "id": "Q25",
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

## The six calls

| Call | Who | What it does |
| --- | --- | --- |
| `PutQuestion` | chief | Adds a question. With an existing id it raises the version and makes the question `waiting` again, with no answer, whatever its state. A waiting or deferred question keeps its place; any other goes last. Returns `id` and `version`. |
| `DismissQuestion` | chief | Withdraws a waiting or deferred question, with a `reason` the dash shows. |
| `ReorderQuestions` | chief | Puts the listed ids that are waiting or deferred first, in that order, keeping their state. Other ids are ignored, so a race with a dismissal is harmless. |
| `ListQuestions` | dash | Returns the waiting questions, then the deferred ones (flagged `deferred`), the count `waiting`, and `dismissed`: the id and reason of every question dismissed in the last hour. |
| `AnswerQuestion` | dash | Answers a question at the version the dash saw. |
| `DeferQuestion` | dash | Sets a question aside. It stays in the queue for a later batch. |

The `waiting` count in `ListQuestions` is the number of questions listed, deferred ones included.

### Answers

`AnswerQuestion` takes `id`, `version`, a `kind`, `picks` and a `note`. A pick has a `decision` (counting from 0), an option `key`, and `text` for a free option.

| Kind | Picks | Note |
| --- | --- | --- |
| `ANSWER` | One per decision, in order, none of them free. | Optional. |
| `FREE_TEXT` | One per decision, in order, at least one of them free. A free pick must have `text`. | Optional. |
| `REVISE` | Any picks made so far, in order. | Required: what to change. |
| `REJECT` | Any picks made so far, in order. | Required: why. |

`UNSPECIFIED` is refused. A free option does not end the walk through the decisions: its typed text becomes that decision's pick, and the dash goes on to the next decision. The answer is `FREE_TEXT` when any pick is free.

### Refusals

| Status | Message | When |
| --- | --- | --- |
| `InvalidArgument` | `invalid <field>` | A rule is broken. The field is named, such as `invalid link`, `invalid decisions.1.options.2.key`, `invalid picks`, `invalid note` or `invalid kind`. Nothing is stored. |
| `FailedPrecondition` | `version_changed` | The question was put again since the dash read it. |
| `FailedPrecondition` | `not_waiting` | The question is dismissed, already answered, or unknown. |
| `Unavailable` | `the question database is unavailable` | A database write failed. |

## The CLI

The chief uses `harold tool questions`. Each command prints one line, or the refusal's message.

```sh
harold tool questions put < question.json        # stored Q25 v1
harold tool questions list                       # one line per question, then "<n> waiting"
harold tool questions order Q27 Q25              # ordered 2
harold tool questions dismiss Q25 --reason "spec withdrawn"   # dismissed Q25
```

`list` prints `<id> v<version> <product> <spec>`, with ` (deferred)` after a deferred one. Any other use prints `invalid argument: usage is harold tool questions put|dismiss|order|list`. If the daemon is not running the CLI prints `the Harold daemon is not reachable`, and after 3 seconds without an answer, `the Harold daemon did not answer within 3 seconds`.

The user answers in the dash, not through the CLI. See the [dash README](../../../tmx-agent-dash/README.md#question-block).

## States

A question is in one state:

- `waiting`: it needs an answer.
- `deferred`: set aside with `DeferQuestion`. It can still be answered.
- `answered`: it has an answer that is not yet in a batch.
- `sent`: its answer was typed to the chief.
- `dismissed`: the chief withdrew it.

`PutQuestion` on a dismissed or sent id makes it `waiting` again at the next version.

## The batch line

A batch starts only if no question is `waiting` (deferred ones do not count), at least one answer is unsent, and the answers are unchanged since they were read. If a question is put again between reading and starting, nothing starts and the sender looks again.

The line is typed into the chief's pane with Enter:

```text
ANSWERS 3 | Q25 durable-server sha d84bf386a2a5: protocol libSQL's Hrana; build yes | Q26 voice-x: revise "make the button bigger" | Q27 old-y: reject "not needed" | later: Q28
```

- `ANSWERS <n>` counts answers, not the deferred ids.
- An `answer` or `free_text` part is `<id> <spec> sha <sha>: <pick>; <pick>`. A pick is `<ask> <label>`. A free option's pick is `<ask> "<typed text>"`, for example `Q28 net-z sha 3333: Which? "use HTTP/3"`.
- A `revise` or `reject` part is `<id> <spec>: revise "<note>"` or `<id> <spec>: reject "<note>"`. It has no `sha`.
- ` | later: <id>, <id>` is added only when some question is deferred.

Answers come in the queue's order.

### Cleaning

Every field is cleaned before it goes into the line, so a question cannot inject keystrokes into the chief's pane:

- `\n` and `\r\n` become a space.
- A lone `\r`, tabs and every other control character are removed.
- An escape sequence is removed: `ESC`, and for `ESC [` everything up to and including the final byte in `0x40` to `0x7E`.

A `|` in a field is not changed, so a note that holds `|` reads like a separator. The chief reads the line by its `ANSWERS <n>` count and the `<id> <spec>` heads.

### Timing and restart

The sender waits for the chief's pane to be idle with an empty input box. If there is no chief, the pane stays busy, or the database fails, it retries every 5 seconds without limit, and logs `answers batch waiting` once per batch with its `reason`.

The batch is stored with its text before it is typed, and marked typed after. Once typed:

- Its answers become `sent`.
- Every deferred question returns to `waiting`, at the end, so the next batch waits for it.

If Enter fails after the text went in, the batch counts as typed and is never typed again, since the text is already in the box.

If Harold stops between typing and marking, the batch is still open at the next start and is typed again, once, before any new batch. A line typed but not marked is the only case that can repeat. A batch whose text was never typed is typed exactly once.

## Log lines

Six fixed lines, with only the fields `tool`, `question_id`, `version` and `reason`. No question or answer text is ever logged.

| Line | Fields | Notes |
| --- | --- | --- |
| `question stored` | `tool`, `question_id`, `version` | |
| `question dismissed` | `tool`, `question_id`, `version` | |
| `question answered` | `tool`, `question_id`, `version` | |
| `answers batch typed` | `tool`, `reason` | `reason` is `typed`, or `enter_failed` when Enter failed after the text went in (a warning). |
| `answers batch waiting` | `tool`, `reason` | `reason` is `no_chief`, `pane_busy`, `no_pane` or `store_failed`. |
| `tool failed to start` | `tool`, `reason` | `reason` is `store_open_failed`. |

## Data folder and rollback

The data folder is `<parent of the store path>/tools/questions/`. It holds `questions.db`, a Turso database with two tables: `questions` (the question, its state and answer, and the dismissal `reason`) and `batches` (each line's text and whether it was typed).

The core store is untouched. To roll back:

- Run an older Harold. It ignores `tools/questions/`.
- Delete `questions.db` to empty the queue. Harold makes a fresh one at the next start.
