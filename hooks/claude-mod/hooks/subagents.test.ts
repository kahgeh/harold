import { test, expect, mock } from 'claude-code/testing'
import type { On } from 'claude-code'

type Row = { id: string; type: string; description: string; status: string }

const ENV = { TMUX_PANE: '%7' }
const COMPLETE = { answer: '', durationMs: 1, isAborted: false, turnId: 't1' }
const START = { text: 'hi', turnId: 't1' }
const MAIN_END = { ...COMPLETE, reason: 'answer' } as const
const subagentEnd = (agentId: string) => ({ ...COMPLETE, agentId, reason: 'answer' }) as const
const row = (id: string, status: string): Row => ({ id, type: 'general-purpose', description: 'task', status })

// The bottom of the test: a clock the test moves, the subagent list the test hands over, and
// the event each `harold accept` run carried.
const bottom = (
  on: On,
  list: () => Row[],
  exitCode: () => number = () => 0,
  onRun: (c: { argv: readonly string[] }) => Promise<void> = async () => {},
) => {
  const sent: string[] = []
  const engine = { turnCompleted: false, listReadAfterTurn: [] as boolean[] }
  const clock = mock.clock(on)
  mock.env(on, ENV)
  on('turn.start', (_$, e) => ({ turnId: e.turnId }))
  on('turn.complete', () => {
    engine.turnCompleted = true
    return { text: 'done' }
  })
  on('agent.list', () => {
    engine.listReadAfterTurn.push(engine.turnCompleted)
    return { value: list() as never }
  })
  on('process.run', async (_$, e) => {
    sent.push(e.argv[2])
    await onRun({ argv: e.argv })
    return { value: { exitCode: exitCode(), stdout: '', stderr: '', isStdoutTruncated: false, isStderrTruncated: false } }
  })
  return { sent, clock, engine }
}

test('every 10 seconds of a main turn reports still-working', async ($, on) => {
  const { sent, clock } = bottom(on, () => [])
  await $.turn.start(START)
  await clock.advance(9_999)
  expect(sent).toEqual(['turn-started'])
  await clock.advance(1)
  await clock.advance(10_000)
  expect(sent).toEqual(['turn-started', 'still-working', 'still-working'])
})

test('a main turn that ends with no subagent working reports turn-completed and nothing after', async ($, on) => {
  const { sent, clock } = bottom(on, () => [row('a1', 'completed')])
  await $.turn.start(START)
  await $.turn.complete(MAIN_END)
  await clock.advance(60_000)
  expect(sent).toEqual(['turn-started', 'turn-completed'])
})

test('a main turn that ends with a subagent working reports still-working, then again every 10 seconds', async ($, on) => {
  const { sent, clock } = bottom(on, () => [row('a1', 'running')])
  await $.turn.start(START)
  await $.turn.complete(MAIN_END)
  expect(sent).toEqual(['turn-started', 'still-working'])
  await clock.advance(20_000)
  expect(sent).toEqual(['turn-started', 'still-working', 'still-working', 'still-working'])
})

test('the last subagent turn to end with no main turn running reports subagents-finished and nothing after', async ($, on) => {
  let rows = [row('a1', 'running')]
  const { sent, clock } = bottom(on, () => rows)
  await $.turn.start(START)
  await $.turn.complete(MAIN_END)
  rows = [row('a1', 'completed')]
  await $.turn.complete(subagentEnd('a1'))
  expect(sent).toEqual(['turn-started', 'still-working'])
  await clock.advance(3_000)
  expect(sent).toEqual(['turn-started', 'still-working', 'subagents-finished'])
  await clock.advance(60_000)
  expect(sent).toEqual(['turn-started', 'still-working', 'subagents-finished'])
})

test('a 10-second check that finds no subagent working reports subagents-finished and nothing after', async ($, on) => {
  let rows = [row('a1', 'running')]
  const { sent, clock } = bottom(on, () => rows)
  await $.turn.start(START)
  await $.turn.complete(MAIN_END)
  rows = [row('a1', 'killed')]
  await clock.advance(60_000)
  expect(sent).toEqual(['turn-started', 'still-working', 'subagents-finished'])
})

test('a subagent that finishes while the main turn is running reports nothing', async ($, on) => {
  const { sent, clock } = bottom(on, () => [row('a1', 'completed')])
  await $.turn.start(START)
  await $.turn.complete(subagentEnd('a1'))
  await clock.advance(3_000)
  expect(sent).toEqual(['turn-started'])
  await clock.advance(10_000)
  // Only the main turn's own check reports, and it reports busy.
  expect(sent).toEqual(['turn-started', 'still-working'])
})

test('one of two subagents finishing keeps the pane on still-working', async ($, on) => {
  let rows = [row('a1', 'running'), row('a2', 'running')]
  const { sent, clock } = bottom(on, () => rows)
  await $.turn.start(START)
  await $.turn.complete(MAIN_END)
  rows = [row('a1', 'completed'), row('a2', 'running')]
  await $.turn.complete(subagentEnd('a1'))
  await clock.advance(10_000)
  expect(sent).not.toContain('subagents-finished')
  expect(sent.at(-1)).toBe('still-working')
})

test('a stopped subagent reports subagents-finished 3 seconds after its turn ends', async ($, on) => {
  let rows = [row('a1', 'running')]
  const { sent, clock } = bottom(on, () => rows)
  await $.turn.start(START)
  await $.turn.complete(MAIN_END)
  rows = [row('a1', 'killed')]
  await $.turn.complete(subagentEnd('a1'))
  await clock.advance(3_000)
  expect(sent).toEqual(['turn-started', 'still-working', 'subagents-finished'])
})

for (const status of ['pending', 'running', 'waiting']) {
  test(`a ${status} subagent keeps the pane busy when the main turn ends`, async ($, on) => {
    const { sent } = bottom(on, () => [row('a1', status)])
    await $.turn.start(START)
    await $.turn.complete(MAIN_END)
    expect(sent).toEqual(['turn-started', 'still-working'])
  })
}

for (const status of ['idle', 'completed', 'failed', 'killed']) {
  test(`a ${status} subagent does not keep the pane busy when the main turn ends`, async ($, on) => {
    const { sent } = bottom(on, () => [row('a1', status)])
    await $.turn.start(START)
    await $.turn.complete(MAIN_END)
    expect(sent).toEqual(['turn-started', 'turn-completed'])
  })
}

test('a subagent waiting on its own running subagent keeps the pane on still-working', async ($, on) => {
  const { sent, clock } = bottom(on, () => [row('a1', 'waiting'), { ...row('a2', 'running'), parentId: 'a1' } as Row])
  await $.turn.start(START)
  await $.turn.complete(MAIN_END)
  await clock.advance(10_000)
  expect(sent).toEqual(['turn-started', 'still-working', 'still-working'])
})

test('a failing subagent list counts as no subagent working', async ($, on) => {
  const { sent, clock } = bottom(on, () => { throw new Error('no list') })
  await $.turn.start(START)
  await clock.advance(10_000)
  expect(sent).toEqual(['turn-started', 'still-working'])
  await $.turn.complete(MAIN_END)
  await clock.advance(60_000)
  expect(sent).toEqual(['turn-started', 'still-working', 'turn-completed'])
})

test('a still-working report that fails is followed by the next one 10 seconds later', async ($, on) => {
  let code = 0
  const { sent, clock } = bottom(on, () => [], () => code)
  await $.turn.start(START)
  code = 1
  await clock.advance(10_000)
  code = 0
  await clock.advance(10_000)
  expect(sent).toEqual(['turn-started', 'still-working', 'still-working'])
})

test('a main turn that starts while a subagent works keeps one check running', async ($, on) => {
  let rows = [row('a1', 'running')]
  const { sent, clock } = bottom(on, () => rows)
  await $.turn.start(START)
  await $.turn.complete(MAIN_END)
  rows = [row('a1', 'completed')]
  await $.turn.start({ text: 'woken', turnId: 't2' })
  await clock.advance(10_000)
  expect(sent).toEqual(['turn-started', 'still-working', 'turn-started', 'still-working'])
  await $.turn.complete({ ...MAIN_END, turnId: 't2' })
  await clock.advance(60_000)
  expect(sent.slice(4)).toEqual(['turn-completed'])
})

test('a main turn that ends with a subagent working, with no start seen, still checks every 10 seconds', async ($, on) => {
  let rows = [row('a1', 'running')]
  const { sent, clock } = bottom(on, () => rows)
  await $.turn.complete(MAIN_END)
  rows = [row('a1', 'completed')]
  await clock.advance(10_000)
  expect(sent).toEqual(['still-working', 'subagents-finished'])
})

test('a subagent turn that ends after the pane was reported idle reports nothing', async ($, on) => {
  const { sent, clock } = bottom(on, () => [row('a1', 'completed')])
  await $.turn.start(START)
  await $.turn.complete(MAIN_END)
  await $.turn.complete(subagentEnd('a1'))
  await clock.advance(3_000)
  await clock.advance(10_000)
  expect(sent).toEqual(['turn-started', 'turn-completed'])
})

test('a main turn that starts while the subagent list is being read is not reported idle', async ($, on) => {
  const sent: string[] = []
  const clock = mock.clock(on)
  mock.env(on, ENV)
  on('turn.start', (_$, e) => ({ turnId: e.turnId }))
  on('turn.complete', () => ({ text: 'done' }))
  on('agent.list', async () => {
    await clock.sleep(50)
    return { value: [] }
  })
  on('process.run', async (_$, e) => {
    sent.push(e.argv[2])
    return { value: { exitCode: 0, stdout: '', stderr: '', isStdoutTruncated: false, isStderrTruncated: false } }
  })
  const ended = $.turn.complete(MAIN_END)
  await clock.settle()
  await $.turn.start(START)
  await clock.advance(50)
  await ended
  expect(sent).toEqual(['turn-started'])
})

test('a main turn that wakes within 3 seconds of the last subagent ending is never reported idle', async ($, on) => {
  let rows = [row('a1', 'running')]
  const { sent, clock } = bottom(on, () => rows)
  await $.turn.start(START)
  await $.turn.complete(MAIN_END)
  rows = [row('a1', 'completed')]
  await $.turn.complete(subagentEnd('a1'))
  await clock.advance(500)
  await $.turn.start({ text: 'woken', turnId: 't2' })
  await clock.advance(5_000)
  expect(sent).toEqual(['turn-started', 'still-working', 'turn-started'])
  await $.turn.complete({ ...MAIN_END, turnId: 't2' })
  await clock.advance(60_000)
  expect(sent.slice(3)).toEqual(['turn-completed'])
})

test('a check that runs inside the 3-second wait does not report idle, and a wake-up cancels the wait', async ($, on) => {
  let rows = [row('a1', 'running')]
  const { sent, clock } = bottom(on, () => rows)
  await $.turn.start(START)
  await $.turn.complete(MAIN_END)
  await clock.advance(9_800)
  rows = [row('a1', 'completed')]
  await $.turn.complete(subagentEnd('a1'))
  await clock.advance(200)
  await clock.advance(200)
  await $.turn.start({ text: 'woken', turnId: 't2' })
  await clock.advance(30_000)
  expect(sent).not.toContain('subagents-finished')
  // The check at 10.0 s, inside the wait, sent nothing; the wake-up's turn-started follows.
  expect(sent.slice(0, 3)).toEqual(['turn-started', 'still-working', 'turn-started'])
})

test('two subagents ending 1 second apart report idle 3 seconds after the second', async ($, on) => {
  let rows = [row('a1', 'running'), row('a2', 'running')]
  const { sent, clock } = bottom(on, () => rows)
  await $.turn.start(START)
  await $.turn.complete(MAIN_END)
  rows = [row('a1', 'completed'), row('a2', 'running')]
  await $.turn.complete(subagentEnd('a1'))
  await clock.advance(1_000)
  rows = [row('a1', 'completed'), row('a2', 'completed')]
  await $.turn.complete(subagentEnd('a2'))
  await clock.advance(2_999)
  expect(sent).not.toContain('subagents-finished')
  await clock.advance(1)
  expect(sent.at(-1)).toBe('subagents-finished')
  expect(sent.filter(e => e === 'subagents-finished')).toHaveLength(1)
})

// A report that ignores the chain would finish first here: only turn-started is slow.
const slowStart = async (c: { argv: readonly string[] }) => {
  if (c.argv.includes('turn-started')) await new Promise(r => setTimeout(r, 50))
}

test('a check queues behind a report still in flight', async ($, on) => {
  const finished: string[] = []
  const { clock } = bottom(on, () => [], undefined, async c => {
    await slowStart(c)
    finished.push(c.argv[2])
  })
  const started = $.turn.start(START)
  await clock.settle()
  await clock.advance(10_000)
  await started
  await new Promise(r => setTimeout(r, 100))
  expect(finished).toEqual(['turn-started', 'still-working'])
})

test('a main turn ending with a subagent working queues behind a report still in flight', async ($, on) => {
  const finished: string[] = []
  bottom(on, () => [row('a1', 'running')], undefined, async c => {
    await slowStart(c)
    finished.push(c.argv[2])
  })
  const started = $.turn.start(START)
  await $.turn.complete(MAIN_END)
  await started
  expect(finished).toEqual(['turn-started', 'still-working'])
})

test('a subagent-finished report queues behind a report still in flight', async ($, on) => {
  const finished: string[] = []
  let rows = [row('a1', 'running')]
  const { clock } = bottom(on, () => rows, undefined, async c => {
    if (c.argv.includes('still-working')) await new Promise(r => setTimeout(r, 50))
    finished.push(c.argv[2])
  })
  await $.turn.start(START)
  await $.turn.complete(MAIN_END)
  rows = [row('a1', 'completed')]
  await $.turn.complete(subagentEnd('a1'))
  await clock.advance(3_000)
  await new Promise(r => setTimeout(r, 100))
  expect(finished).toEqual(['turn-started', 'still-working', 'subagents-finished'])
})

test('a throwing timer cannot lose the turn-started report', async ($, on) => {
  const sent: string[] = []
  mock.env(on, ENV)
  on('turn.start', (_$, e) => ({ turnId: e.turnId }))
  on('clock.every', () => {
    throw new Error('no timers')
  })
  on('process.run', async (_$, e) => {
    sent.push(e.argv[2])
    return { value: { exitCode: 0, stdout: '', stderr: '', isStdoutTruncated: false, isStderrTruncated: false } }
  })
  await $.turn.start(START)
  expect(sent).toEqual(['turn-started'])
})

test('a steady stream of subagent ends keeps the pane on still-working every 10 seconds', async ($, on) => {
  const { sent, clock } = bottom(on, () => [row('a1', 'running'), row('a2', 'completed')])
  await $.turn.start(START)
  await $.turn.complete(MAIN_END)
  for (let at = 2; at <= 40; at += 2) {
    await clock.advance(2_000)
    await $.turn.complete(subagentEnd('a2'))
  }
  expect(sent).toEqual(['turn-started', ...Array(5).fill('still-working')])
})

test('the wait firing with another subagent working leaves the checks able to report idle', async ($, on) => {
  let rows = [row('a1', 'running'), row('a2', 'running')]
  const { sent, clock } = bottom(on, () => rows)
  await $.turn.start(START)
  await $.turn.complete(MAIN_END)
  rows = [row('a1', 'completed'), row('a2', 'running')]
  await $.turn.complete(subagentEnd('a1'))
  await clock.advance(3_000)
  expect(sent.at(-1)).toBe('still-working')
  // a2 ends with no event reaching the mod; the next check at 10 s notices.
  rows = [row('a1', 'completed'), row('a2', 'completed')]
  await clock.advance(7_000)
  expect(sent.at(-1)).toBe('subagents-finished')
})

test('the subagent list is read after the engine has handled the main turn end', async ($, on) => {
  const { engine } = bottom(on, () => [row('a1', 'completed')])
  await $.turn.start(START)
  await $.turn.complete(MAIN_END)
  expect(engine.listReadAfterTurn).toEqual([true])
})
