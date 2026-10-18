# Harold Docs

## How-Tos

- [Setup](how-tos/setup.md)
- [Setup Agent Monitor Hooks](how-tos/setup-agent-monitor-hooks.md)
- [Setup Telegram](how-tos/setup-telegram.md)

## Prerequisites

- [Prerequisites](prerequisites.md)

## References

- [Agent Monitor](references/agent-monitor/README.md)
- [Notification](references/notification/README.md)
- [Inbound Message Routing](references/inbound-message-routing/README.md)
- [Operation](references/operation/README.md)
- [Tools](references/tools/README.md)

## Explanations

- [Architecture](explanations/architecture.md)
- [Channel Security: iMessage vs Telegram](explanations/channel-security.md)

## Decisions

- [1. Tools are modules that Harold hosts](adr/0001-tools-are-modules-hosted-by-harold.md)
- [2. Tool services trust local callers](adr/0002-tool-services-trust-local-callers.md)
- [3. Pod merge counts save the counted tip before recording](adr/0003-pod-merge-counts-save-the-tip-first.md)
