# Harold Docs

## How-Tos

- [Setup](how-tos/setup.md)
- [Setup Agent Monitor Hooks](how-tos/setup-agent-monitor-hooks.md)
- [Release](how-tos/release.md)

## Prerequisites

- [Prerequisites](prerequisites.md)

## Glossary

- [Glossary](glossary.md)

## References

- [Agent Monitor](references/agent-monitor/README.md)
- [Notification](references/notification/README.md)
- [Inbound Message Routing](references/inbound-message-routing/README.md)
- [Operation](references/operation/README.md)
- [Tools](references/tools/README.md)
- [Dependency Audits](references/dependency-audits.md)

## Explanations

- [Architecture](explanations/architecture.md)
- [Channel Security: iMessage](explanations/channel-security.md)

## Decisions

- [1. Tools are modules that Harold hosts](adr/0001-tools-are-modules-hosted-by-harold.md)
- [2. Tool services trust local callers](adr/0002-tool-services-trust-local-callers.md)
- [3. Pod merge counts save the counted tip before recording](adr/0003-pod-merge-counts-save-the-tip-first.md)
- [24. Agents have six states, and Harold takes them from the terminal when the agent reports them](adr/0024-agents-have-six-states-and-harold-takes-them-from-the-terminal-when-it-reports-them.md)
- [25. Harold keeps transcript text in memory and shows it only in the control-tower](adr/0025-harold-keeps-transcript-text-in-memory-and-shows-it-only-in-the-control-tower.md)
- [27. Harold reads agents on other Rex hosts, read-only, through Rex](adr/0027-harold-reads-agents-on-other-rex-hosts-read-only-through-rex.md)
- [26. A release publishes only a commit whose Linux tests passed](adr/0026-a-release-publishes-only-a-commit-whose-linux-tests-passed.md)
