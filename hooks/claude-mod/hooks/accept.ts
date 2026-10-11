export type ReportEvent = 'turn-started' | 'still-working' | 'turn-completed' | 'subagents-finished'

// A subagent in one of these states has work under way.
const WORKING_STATUSES: readonly string[] = ['pending', 'running', 'waiting']

export const hasWorking = (agents: readonly { status: string }[]): boolean =>
  agents.some(agent => WORKING_STATUSES.includes(agent.status))

export const isPaneId = (value: string | undefined): value is string =>
  value !== undefined && /^%[0-9]+$/.test(value)

// The installer copies the mod to <prefix>/harold/hooks/claude-mod, beside the binary.
export const resolveProgram = ({ bin, root }: { bin: string | undefined; root: string }): string =>
  bin?.trim() || `${root}/../../harold`

export const buildArgv = ({
  program,
  event,
  paneId,
}: {
  program: string
  event: ReportEvent
  paneId: string
}): string[] => [program, 'accept', event, '--pane', paneId, '--adapter', 'claude.lifecycle']
