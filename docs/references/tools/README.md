# Tools

A tool is a small service inside the Harold daemon that does one job for the chief of pods and keeps its own storage. The first tool is the [question queue](questions.md). This page is for a developer adding the next one.

## The contract

A tool is a module under `harold/src/tools/<name>/`. Its public surface is a `NAME` and one function:

```rust
pub(crate) async fn start(ctx: ToolContext) -> Result<ToolHandle, ToolError>
```

The host (`harold/src/tools.rs`) builds the `ToolContext` and calls `start`. The context holds four things:

| Field | What it is |
| --- | --- |
| `data_folder` | The tool's own folder, `<parent of the store path>/tools/<name>/`. The tool opens its own database there. |
| `typing` | A `PaneTyping`: makes one try at typing one line into a pane, which must be idle with an empty input box. It never waits for a busy pane; the tool tries again on a later round. |
| `chief_pane` | A `ChiefPane`: the pane id of the chief, if one is running. |
| `shutdown` | A `watch::Receiver<()>` that fires when Harold stops. |

`start` gives back a `ToolHandle`: the gRPC `routes` the host adds to its server, and the `task` the host joins on shutdown. The task must end when `shutdown` fires.

`PaneTyping::type_line` returns a `Typing` at once: `Typed`, `EnterFailed` (the text is in the input box but Enter failed, so typing it again would repeat it), `PaneBusy` (the pane is busy or its input box holds text; nothing was typed) or `NoPane`. It holds the pane against Harold's other writers as `Writer::Tool(name)`. Today the host builds one `TmuxTyping` with `Writer::Tool(questions::NAME)` and gives it to the one tool it starts, so the writer is the question tool's. Two tools with different names would hold the pane against each other like any two writers, but only if the host gives each its own writer. It logs nothing; the tool logs its own lines.

`ChiefPane` finds the one agent pane whose folder is `[notify] away_messages_only_from`. With no folder set, no such pane, or more than one, there is no chief and the tool should wait.

## The boundary

Outside `harold/src/tools/<name>/`, only `start` and `NAME` are used. The host names no database type and no tool internals. A tool may change its schema, its storage or its service without touching the core.

A tool never touches the core store (`harold/src/store.rs`, `harold/src/store/`). It opens its own database in `data_folder`.

## The CLI group

`harold tool <name> ...` is the command group for tools. Each tool adds its own subcommands. The question tool's are in [questions.md](questions.md#the-cli).

## Off switch and failure

A tool that cannot start is logged and left out; Harold runs on without its service. The line is:

```text
tool failed to start    tool=questions reason=store_open_failed
```

`reason` is a fixed word, never error text or a path. `ToolError` carries only a `&'static str` for this reason. Callers of the missing service get `Unimplemented`, and the CLI prints `the Harold daemon does not serve questions`.

There is no setting that turns a tool off. To stop one, remove its `start_tool` line from the host.

## Adding a tool

1. Write the module under `harold/src/tools/<name>/` with `NAME` and `start`. Open storage in `ctx.data_folder`.
2. Add a proto under `harold-api/proto/tools/<name>.proto`, and build it in `harold-api/build.rs`.
3. Add `pub(crate) mod <name>;` and one `start_tool` call in the host's `start_with`, with `data_folder(store_path, <name>::NAME)`.
4. Give the tool its own writer. The host does not do this for you: build a `TmuxTyping { writer: Writer::Tool(<name>::NAME), .. }` for the new tool, and its own `ChiefPane` if it needs a different lookup, and put them in its `ToolContext`. Reusing the question tool's `typing` would type as `Tool("questions")`.
5. Add a CLI subcommand under `harold tool <name>`.
6. Log with fixed lines and the fields `tool`, `question_id` or another id, `version` and `reason`. Never log user text.
7. Write a page here, as `docs/references/tools/<name>.md`.

## The next tool

The next tool is each product's backlog, plus the chief's own queue. Its scope is the user's: what each product still has to do, and what the chief itself has queued. It is not designed yet.

## Tools

- [Question queue](questions.md)
