import type { EngineInterface, Register } from 'claude-code'
import { buildArgv, isPaneId, resolveProgram, type TurnEvent } from './accept'

// Reports leave one at a time, in the order the events fired. A report that had to wait behind
// one that failed or timed out is skipped, so a turn is held for one bounded call, not several.
let chain: Promise<boolean> = Promise.resolve(true)
let isBusy = false

async function report($: EngineInterface, event: TurnEvent): Promise<boolean> {
  try {
    const paneId = await $.env.get('TMUX_PANE')
    if (!isPaneId(paneId)) return true

    const program = resolveProgram({ bin: await $.env.get('HAROLD_BIN'), root: $.plugin.root })
    const { exitCode } = await $.process.run(buildArgv({ program, event, paneId }), { timeoutMs: 4000 })

    return exitCode === 0
  } catch {
    // Fail open: a missing program, a dead daemon or a timeout never reaches the session.
    return false
  }
}

function enqueue($: EngineInterface, event: TurnEvent): Promise<boolean> {
  const waited = isBusy
  const mine = chain.then(previousWorked => (waited && !previousWorked ? false : report($, event)))
  isBusy = true
  chain = mine
  void mine.then(() => {
    if (chain === mine) isBusy = false
  })

  return mine
}

const agentIdOf = (e: object): string | undefined => (e as { agentId?: string }).agentId

export const register: Register = on => {
  on('turn.start', async ($, e, next) => {
    const sent = agentIdOf(e) === undefined ? enqueue($, 'turn-started') : undefined
    const result = await next(e)
    await sent

    return result
  })

  on('turn.complete', async ($, e, next) => {
    const sent = agentIdOf(e) === undefined ? enqueue($, 'turn-completed') : undefined
    const result = await next(e)
    await sent

    return result
  })
}
