import type { EngineInterface, Register } from 'claude-code'
import { buildArgv, isPaneId, resolveProgram, type TurnEvent } from './accept'

// Reports leave one at a time, in the order the events fired.
let chain: Promise<void> = Promise.resolve()

async function report($: EngineInterface, event: TurnEvent): Promise<void> {
  try {
    const paneId = await $.env.get('TMUX_PANE')
    if (!isPaneId(paneId)) return

    const program = resolveProgram({ bin: await $.env.get('HAROLD_BIN'), root: $.plugin.root })

    await $.process.run(buildArgv({ program, event, paneId }), { timeoutMs: 4000 })
  } catch {
    // Fail open: a missing program, a dead daemon or a timeout never reaches the session.
  }
}

function enqueue($: EngineInterface, event: TurnEvent): Promise<void> {
  chain = chain.then(() => report($, event))

  return chain
}

export const register: Register = on => {
  on('turn.start', async ($, e, next) => {
    const sent = 'agentId' in e ? undefined : enqueue($, 'turn-started')
    const result = await next(e)
    await sent

    return result
  })

  on('turn.complete', async ($, e, next) => {
    const sent = e.agentId === undefined ? enqueue($, 'turn-completed') : undefined
    const result = await next(e)
    await sent

    return result
  })
}
