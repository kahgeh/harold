import type { EngineInterface, Register, Timer } from 'claude-code'
import { buildArgv, hasWorking, isPaneId, resolveProgram, type ReportEvent } from './accept'

const CHECK_EVERY_MS = 10_000

// Reports leave one at a time, in the order the events fired. A report that had to wait behind
// one that failed or timed out is skipped, so a turn is held for one bounded call, not several.
let chain: Promise<boolean> = Promise.resolve(true)
let isBusy = false
let isMainTurnRunning = false
// Runs from the start of a main turn until the pane is reported idle.
let checks: Timer | undefined

async function report($: EngineInterface, event: ReportEvent): Promise<boolean> {
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

function enqueue($: EngineInterface, event: ReportEvent): Promise<boolean> {
  const waited = isBusy
  const mine = chain.then(previousWorked => (waited && !previousWorked ? false : report($, event)))
  isBusy = true
  chain = mine
  void mine.then(() => {
    if (chain === mine) isBusy = false
  })

  return mine
}

async function hasWorkingSubagent($: EngineInterface): Promise<boolean> {
  try {
    return hasWorking(await $.agent.list())
  } catch {
    // A failing list counts as no subagent working, so a pane cannot stay busy on an error.
    return false
  }
}

function startChecks($: EngineInterface): void {
  if (checks !== undefined) return
  checks = $.clock.every(CHECK_EVERY_MS, () => {
    void check($)
  })
}

function stopChecks(): void {
  checks?.cancel()
  checks = undefined
}

// Called when a main turn or a subagent's turn has ended, and at a check with no main turn
// running. Reports busy while a subagent works; otherwise reports idleEvent and stops the checks.
async function reportAfterEnd($: EngineInterface, idleEvent: ReportEvent): Promise<boolean> {
  const isSubagentWorking = await hasWorkingSubagent($)
  if (isMainTurnRunning) return true
  // Another path reported the pane idle while the list was being read.
  if (idleEvent === 'subagents-finished' && checks === undefined) return true
  if (isSubagentWorking) {
    startChecks($)

    return enqueue($, 'still-working')
  }
  stopChecks()

  return enqueue($, idleEvent)
}

async function check($: EngineInterface): Promise<void> {
  if (isMainTurnRunning) {
    await enqueue($, 'still-working')
    return
  }
  await reportAfterEnd($, 'subagents-finished')
}

const agentIdOf = (e: object): string | undefined => (e as { agentId?: string }).agentId

export const register: Register = on => {
  on('turn.start', async ($, e, next) => {
    let sent: Promise<boolean> | undefined
    if (agentIdOf(e) === undefined) {
      isMainTurnRunning = true
      startChecks($)
      sent = enqueue($, 'turn-started')
    }
    const result = await next(e)
    await sent

    return result
  })

  on('turn.complete', async ($, e, next) => {
    let sent: Promise<boolean> | undefined
    if (agentIdOf(e) === undefined) {
      isMainTurnRunning = false
      sent = reportAfterEnd($, 'turn-completed')
    } else if (!isMainTurnRunning && checks !== undefined) {
      sent = reportAfterEnd($, 'subagents-finished')
    }
    const result = await next(e)
    await sent

    return result
  })
}
