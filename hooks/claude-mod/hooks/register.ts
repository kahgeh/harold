import type { EngineInterface, Register, Timer } from 'claude-code'
import { buildArgv, hasWorking, isPaneId, resolveProgram, type ReportEvent } from './accept'

const CHECK_EVERY_MS = 10_000
// After a subagent's turn ends, Claude Code may wake the main session at once. Waiting this long
// before reporting idle keeps that wake-up from showing as idle in between.
const SETTLE_MS = 3_000

// Reports leave one at a time, in the order the events fired. A report that had to wait behind
// one that failed or timed out is skipped, so a turn is held for one bounded call, not several.
let chain: Promise<boolean> = Promise.resolve(true)
let isBusy = false
let isMainTurnRunning = false
// Runs from the start of a main turn until the pane is reported idle.
let checks: Timer | undefined
// The one pending wait after a subagent's turn ended; each new subagent end replaces it.
let settle: Timer | undefined

// A Rex pane id is `block:` and the block's id.
const isRexBlock = (value: string | undefined): value is string =>
  value !== undefined && /^block:[0-9A-Za-z-]+$/.test(value)

// The caller's pane: `TMUX_PANE` in a tmux pane, `REX_BLOCK` in a Rex pane.
async function callerPane($: EngineInterface): Promise<string | undefined> {
  const tmux = await $.env.get('TMUX_PANE')
  if (isPaneId(tmux)) return tmux
  const rex = await $.env.get('REX_BLOCK')
  return isRexBlock(rex) ? rex : undefined
}

async function report($: EngineInterface, event: ReportEvent): Promise<boolean> {
  try {
    const paneId = await callerPane($)
    if (paneId === undefined) return true

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
  cancelSettle()
}

function cancelSettle(): void {
  settle?.cancel()
  settle = undefined
}

// Called when a main turn or a subagent's turn has ended, and at a check with no main turn
// running. Reports busy while a subagent works; otherwise reports idleEvent and stops the checks.
async function reportAfterEnd($: EngineInterface, idleEvent: ReportEvent): Promise<boolean> {
  const isSubagentWorking = await hasWorkingSubagent($)
  if (isMainTurnRunning) return true
  // Another path reported the pane idle while the list was being read.
  if (idleEvent === 'subagents-finished' && checks === undefined) return true
  // A subagent just ended: leave the idle report to the wait, which a wake-up can cancel.
  if (idleEvent === 'subagents-finished' && !isSubagentWorking && settle !== undefined) return true
  if (isSubagentWorking) {
    tryStartChecks($)

    return enqueue($, 'still-working')
  }
  stopChecks()

  return enqueue($, idleEvent)
}

// Reports a subagent's end once nothing has woken the main session for SETTLE_MS. Each end
// restarts the one wait, and a check does not report idle while it is pending (it still
// reports busy).
function settleAfterSubagentEnd($: EngineInterface): void {
  cancelSettle()
  settle = $.clock.after(SETTLE_MS, () => {
    settle = undefined
    void reportAfterEnd($, 'subagents-finished')
  })
}

async function check($: EngineInterface): Promise<void> {
  if (isMainTurnRunning) {
    await enqueue($, 'still-working')
    return
  }
  await reportAfterEnd($, 'subagents-finished')
}

// A timer that cannot start must not lose the report that goes with it.
function tryStartChecks($: EngineInterface): void {
  try {
    startChecks($)
  } catch {
    // Fail open, as every report does.
  }
}

const agentIdOf = (e: object): string | undefined => (e as { agentId?: string }).agentId

export const register: Register = on => {
  on('turn.start', async ($, e, next) => {
    let sent: Promise<boolean> | undefined
    if (agentIdOf(e) === undefined) {
      isMainTurnRunning = true
      cancelSettle()
      tryStartChecks($)
      sent = enqueue($, 'turn-started')
    }
    const result = await next(e)
    await sent

    return result
  })

  on('turn.complete', async ($, e, next) => {
    const isMain = agentIdOf(e) === undefined
    if (isMain) isMainTurnRunning = false
    const result = await next(e)
    // The subagent list is read after the engine has finished with the turn.
    let sent: Promise<boolean> | undefined
    if (isMain) {
      sent = reportAfterEnd($, 'turn-completed')
    } else if (!isMainTurnRunning && checks !== undefined) {
      settleAfterSubagentEnd($)
    }
    await sent

    return result
  })
}
