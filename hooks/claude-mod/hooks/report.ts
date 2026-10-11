export type AgentState = 'AGENT_STATE_BUSY' | 'AGENT_STATE_IDLE'

export type ReportRequest = {
  paneId: string
  state: AgentState
  adapterId: string
}

export type Config = { address: string; protoPath: string }

const DEFAULT_ADDRESS = 'localhost:50060'

export const isPaneId = (value: string | undefined): value is string =>
  value !== undefined && /^%[0-9]+$/.test(value)

export const buildRequest = (paneId: string, state: AgentState): ReportRequest => ({
  paneId,
  state,
  adapterId: 'claude.lifecycle',
})

export const buildInvocation = (
  request: ReportRequest,
  { address, protoPath }: Config,
): { argv: string[]; stdin: string } => {
  const slash = protoPath.lastIndexOf('/')
  const dir = slash < 0 ? '.' : slash === 0 ? '/' : protoPath.slice(0, slash)
  const file = protoPath.slice(slash + 1)

  return {
    argv: [
      'grpcurl', '-plaintext', '-max-time', '3',
      '-import-path', dir, '-proto', file,
      '-d', '@', address, 'harold.Harold/ReportAgentState',
    ],
    stdin: JSON.stringify(request),
  }
}

export const resolveConfig = ({
  addr,
  proto,
  home,
}: {
  addr: string | undefined
  proto: string | undefined
  home: string | undefined
}): Config => ({
  address: addr?.trim() || DEFAULT_ADDRESS,
  protoPath: proto?.trim() || `${home ?? ''}/bin/harold/harold.proto`,
})
