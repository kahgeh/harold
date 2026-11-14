# Pod Tool

The **pod tool** tells which agent holds a **pod role** now: `chief`, `pnem` or `pod_coordination`. It answers from the **pod role record**, which a start hook fills. The tool is a [tool](README.md) with a data folder of its own, `tools/pod/`, in Harold's data folder ([ADR 0001](../../adr/0001-tools-are-modules-hosted-by-harold.md)). It trusts local callers ([ADR 0002](../../adr/0002-tool-services-trust-local-callers.md)). The reasons for the design are in [ADR 0040](../../adr/0040-harold-keeps-a-record-of-the-agent-in-each-pod-role.md).

The service is `harold.tools.pod.v1.PodService`, with the calls `Who` and `Check`. The proto is `harold-api/proto/tools/pod.proto`. The record is filled by the RPC `harold.Harold/PodAgentStarted`, which is part of `harold-api/proto/harold.proto`.

## The command

```sh
harold tool pod who harold pnem
harold tool pod who - chief --json
```

| Command | What it does |
| --- | --- |
| `who <product> <role> [--json]` | Prints the session name of the agent that holds the role, for example `main-b8`. `--json` prints the session name and the whole record. The product of the chief is `-`. |
| `check <product> <role> [--spec <id>]` | Runs the restart check of one agent now and prints its text. The text is the text of the restart receipt file. A build worker has the role `build_worker` and needs `--spec`. |

The command exits 1 and prints one reason word when it finds no agent:

| Reason | Meaning |
| --- | --- |
| `no_record` | No agent reported this role. |
| `agent_gone` | The process of the recorded agent ended, or its pid now runs another process (the start time differs). |
| `session_file_unreadable` | The agent's session file is missing, is not JSON, or lacks `pid`, `procStart` or `name`. |
| `bad_role` | The role is not `chief`, `pnem` or `pod_coordination` (for `check`, also `build_worker`). |
| `bad_spec` | `check` got a build worker with no spec id, or a pod role with a spec id. |
| `restart_check_off` | The Harold daemon did not start the restart check. |

No session name and no folder is matched: a role is found by the record only.

## The record

The `SessionStart` hook `hooks/providers/claude_session_start.py` runs when an agent starts with `POD_ROLE` set. It calls `PodAgentStarted` with `pod_role`, `pod_product`, `pane_id`, `session_id`, `transcript_path`, `cwd`, `agent_pid` (the first process named `claude` among the hook's parents, at most 5 levels up, else the hook's parent), `start_file` and `source`. Without `POD_ROLE`, the hook does nothing. When Harold does not answer within 3 seconds, the hook exits 0, so an agent never waits on it.

The hook takes the pane id from `TMUX_PANE`, then `REX_BLOCK`. With neither, `pane_id` is empty: Harold keeps the record, `Who` still answers by the pid, and `pick_chief` skips a record with no pane id.

Harold keeps one record for each (product, role, spec id) in `pod-roles.json`. The spec id is empty for a pod role. A build worker has the role value `build_worker` and its spec id, which the hook reads from `POD_SPEC` and sends as `pod_spec`. A record of a pod role keeps the key `product/role`; a build worker's key is `product/build_worker/spec`. Each record holds: `pane_id`, `session_id`, `agent_pid`, `agent_proc_start`, `folder`, `transcript_path`, `start_file` and `started_at_ms`. A later start of the same role replaces the record, so the newest agent holds the role. On the sources `clear` and `compact`, Harold only updates `session_id`: the other fields keep their values. A call with `agent_pid` 0 is refused with `invalid_argument`.

The command `scripts/claude-start <model> <prompt file>` starts the agent with the short first message `Read <prompt file> and follow it.` It refuses a prompt file that does not exist.

## Known risks

- A nested agent that inherits `POD_ROLE` and `POD_PRODUCT` from a role's shell replaces the record when it starts. `Who` then gives `agent_gone` when that agent ends. Start no agent from the shell of a role without `env -u POD_ROLE -u POD_PRODUCT`.
- A record with `agent_proc_start` 0 (Harold could not read the start time at the start) matches any live process with the same pid. A reused pid then reads as the agent.

## The start file

The chief's `bin/pod start` writes `<product repo>/.superpowers/pod/start/<role>.json`, and `bin/up` writes `<chief folder>/.superpowers/pod/start/chief.json`. It holds `role`, `product`, `folder`, `model`, `prompt_file`, `env` and `pane_label`. The hook sends its path as `start_file` when the file exists in the folder of the agent. Harold only reads it and writes no file in the pod folder.

## The chief's pane

`pick_chief` takes the chief's pane from the record first, when an agent runs in that pane. Without a usable record it uses `notify.chief_session`, and then the oldest session in the folder. The log line `chief pane changed` then has `session_by = record`.

## Telemetry

- Counter `harold.pod_role.lookups` (`role`, `outcome`, `caller`). `caller` is `who` or `pick_chief`. Each series starts at 0.
- Log lines: `pod role recorded` (`product`, `role`, `spec.id`, `pane_id`, `session.id`, `replaced`) and `pod role lookup` (`product`, `role`, `outcome`). No line holds a session name or a folder.
- The evidence query `evidence/pod-role-record-1.sql` counts the faults `no_record` and `session_file_unreadable`. `agent_gone` is no fault: it is expected while an agent is stopped.

## The restart check

`Check` runs the restart check of one agent: the identity and communication groups of stage 1. The same check runs by itself at the end of the first turn of a restart. A restart is a role start whose role held another session before, with the hook source `startup` or `resume`. The check writes the restart receipt file, sends each FAIL to the agent that fixes it and counts each row on `harold.restart_check.results`. See the [pod roles reference](../pod-roles/README.md) for the rules of the checks.
