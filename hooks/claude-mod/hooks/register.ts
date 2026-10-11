import type { EngineInterface, Register } from 'claude-code'
import { buildInvocation, buildRequest, isPaneId, resolveConfig, type AgentState } from './report'

// Reports leave one at a time, in the order the events fired.
let chain: Promise<void> = Promise.resolve()

async function report($: EngineInterface, state: AgentState): Promise<void> {
  try {
    const paneId = await $.env.get('TMUX_PANE')
    if (!isPaneId(paneId)) return

    const config = resolveConfig({
      addr: await $.env.get('HAROLD_ADDR'),
      proto: await $.env.get('HAROLD_PROTO'),
      home: await $.env.get('HOME'),
    })
    const { argv, stdin } = buildInvocation(buildRequest(paneId, state), config)

    await $.process.run(argv, { stdin, timeoutMs: 4000 })
  } catch {
    // Fail open: Harold down, grpcurl missing or a timeout never reaches the session.
  }
}

function enqueue($: EngineInterface, state: AgentState): Promise<void> {
  chain = chain.then(() => report($, state))

  return chain
}

export const register: Register = on => {
  on('turn.start', async ($, e, next) => {
    const sent = 'agentId' in e ? undefined : enqueue($, 'AGENT_STATE_BUSY')
    const result = await next(e)
    await sent

    return result
  })

  on('turn.complete', async ($, e, next) => {
    const sent = e.agentId === undefined ? enqueue($, 'AGENT_STATE_IDLE') : undefined
    const result = await next(e)
    await sent

    return result
  })
}
