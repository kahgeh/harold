# Tools

A tool is a small service inside the Harold daemon that does one job for the agents and keeps its own storage. There are four: the [question tool](questions.md), the [backlog and queue](backlog.md), the [watch tool](watch.md) and the [knowledge tool](knowledge.md). This page is for a developer adding another.

## The contract

A tool is a module under `harold/src/tools/<name>/`. Its public surface is a `NAME` and one function:

```rust
pub(crate) async fn start(ctx: ToolContext) -> Result<ToolHandle, ToolError>
```

The host (`harold/src/tools.rs`) builds the `ToolContext` and calls `start`. The context holds these:

| Field | What it is |
| --- | --- |
| `data_folder` | The tool's own folder, `<parent of the store path>/tools/<name>/`. The tool opens its own database there. |
| `typing` | An `Option<Arc<dyn PaneTyping>>`. A `PaneTyping` makes one try at typing one line into a pane, which must be idle with an empty input box. It never waits for a busy pane; the tool tries again on a later round. A tool that doesn't type gets `None`; the question tool refuses to start with `None`, with the reason `no_typing`. |
| `chief_pane` | A `ChiefPane`: the pane id of the chief, if one is running. |
| `pane_folder` | A `PaneFolder`: `folder_of(pane_id)` gives the folder of the agent in that pane, from the agent snapshot, or `None` when the pane is not an agent's. |
| `products` | A `Products`: `products()` gives the products of the pod registry (`[telemetry] pod_registry`, the vault's `products.toml`), each as its name and its `repo` folder. It reads the registry on each call, so an edit shows at once. With no `pod_registry` set the list is empty; a registry that is set but can't be read gives `Err("registry_unreadable")`. |
| `agents` | An `Agents`, read only ([ADR 0004](../../adr/0004-tools-may-read-agents-and-pods.md)): `agents()` gives a copy of Harold's agent list, each pane with its folder, provider, state, when it entered that state and its work summary, and `input_box(pane)` reads the pane's Claude Code input box afresh. |
| `pods` | A `Pods`, read only: `pods()` gives a copy of pod stats' latest snapshot, the slots and specs of each product. |
| `watch_settings`, `model_environment`, `cli_path` | The `[watch]` settings, and the environment and program path Harold uses to start Claude Code. Only the watch tool reads them. |
| `shutdown` | A `watch::Receiver<()>` that fires when Harold stops. |

`start` gives back a `ToolHandle`: the gRPC `routes` the host adds to its server, and `task`, the background job the host joins on shutdown. The job must end when `shutdown` fires.

`PaneTyping::type_line` returns a `Typing` at once: `Typed`, `EnterFailed` (the text is in the input box but Enter failed, so typing it again would repeat it), `PaneBusy` (the pane is busy or its input box holds text; nothing was typed) or `NoPane`. It holds the pane against Harold's other writers as `Writer::Tool(name)`. The host builds a `TmuxTyping` with `Writer::Tool(questions::NAME)` for the question tool and another with `Writer::Tool(watch::NAME)` for the watch tool; the backlog tool types nothing and gets `None`. Two tools with different names would hold the pane against each other like any two writers, but only if the host gives each its own writer. It logs nothing; the tool logs its own lines.

`ChiefPane` finds the first agent pane of the chief's tmux session among the panes whose folder `[notify] chief` matches. With no folder set or no such pane, there is no chief and the tool should wait.

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

`reason` is a fixed word, never error text or a path. `ToolError` carries only a `&'static str` for this reason. Callers of the missing service get `Unimplemented`, and the CLI prints `the Harold daemon does not serve questions`, or `the Harold daemon does not serve the backlog`.

The knowledge tool is the one tool with an off switch: it starts only when `[knowledge] vault` is set, and its host call takes a `Launch` (its settings and this Mac's name) beside the context, because it opens a database that needs a bucket. Every other tool has no setting that turns it off; to stop one, remove its `start_tool` line from the host.

## Adding a tool

1. Write the module under `harold/src/tools/<name>/` with `NAME` and `start`. Open storage in `ctx.data_folder`.
2. Add a proto under `harold-api/proto/tools/<name>.proto`, and build it in `harold-api/build.rs`.
3. Add `pub(crate) mod <name>;` and one `start_tool` call in the host's `start_with`, with `data_folder(store_path, <name>::NAME)`.
4. Give a writer only to a tool that types. A tool that types needs its own, and the host does not make one for you: build a `TmuxTyping { writer: Writer::Tool(<name>::NAME), .. }` for the new tool and put it in its `ToolContext` as `Some`, with its own `ChiefPane` if it needs a different lookup. Reusing the question tool's `typing` would type as `Tool("questions")`. A tool that types nothing gets `typing: None`.
5. Add a CLI subcommand under `harold tool <name>`.
6. Log with fixed lines and the fields `tool`, `question_id` or another id, `version` and `reason`. Never log user text.
7. Write a page here, as `docs/references/tools/<name>.md`.

## Tools

- [Question tool](questions.md): the chief's questions for the user, answered from the dash and typed back into the chief's pane.
- [Backlog and queue](backlog.md): each product's ranked backlog, kept by its product manager, and the chief's own queue.
- [Watch tool](watch.md): an agent asks Harold to run one of its skills with a model on a schedule, and Harold types the reports into the chief's pane or the agent's.
- [Knowledge tool](knowledge.md): Harold's index of the organisation's vault of Markdown notes by meaning, for the chief, the web app's Notes page and the dash.
