# 25. Harold reads agents on other Rex hosts, read-only, through Rex

Date: 2026-11-02
Status: Accepted (rex-other-hosts). Builds on 0022.

## Context

Workers run on omarchy under the Mac's pod-coordination, and the control-tower must show them. Rex reads omarchy's panes from the Mac with `rex -S`. The user chose Rex as the carrier for the workers' lines, not a Harold link.

## Decision

1. Harold reads the agents of the Rex hosts named in `[agent_monitor] rex_hosts`, only with Rex, and only when `multiplexer = "rex"`.
2. Harold only reads. It types nothing, reads no screen and no transcript there, and keeps the agents in memory, outside `AgentSnapshot`, the reducer and the store.
3. An agent on a Rex host has the pane id `rex:<label>/block:<id>`. The local forms `%<n>` and `block:<id>` do not change and mean this machine. Every writer goes through one route, `route_pane`, that takes only a local form, by its shape. It refuses every other id before any call.
4. Each host's calls use a 2-second limit. After 3 failed listings Harold tries that host only every 30 seconds. The Mac's scan never waits for a host.
5. Harold reads the state of an agent on a Rex host with one `program_status` call for each agent block in each scan. It starts no `rex events` stream over the network.

## Consequences

An agent on a Rex host has its state and current step in the control-tower, in its pod's group, and nothing else from Harold. A Harold restart, or a renamed label, starts its incarnations again. Typing there waits for a Harold on that host (linux-satellite). The poll in decision 5 costs one more call for each agent in each scan.
