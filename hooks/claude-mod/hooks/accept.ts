export type TurnEvent = 'turn-started' | 'turn-completed'

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
  event: TurnEvent
  paneId: string
}): string[] => [program, 'accept', event, '--pane', paneId, '--adapter', 'claude.lifecycle']
