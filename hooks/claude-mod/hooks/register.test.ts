import { test, expect, mock } from 'claude-code/testing'
import type { On } from 'claude-code'

type Call = { argv: readonly string[]; timeoutMs?: number }

const ENV = { TMUX_PANE: '%7' }
const COMPLETE = { answer: '', durationMs: 1, isAborted: false, turnId: 't1' }

// The bottom of the test: what the engine would answer.
const bottom = (on: On, env: Record<string, string>, run: (c: Call) => Promise<{ exitCode: number }>) => {
  const calls: Call[] = []
  mock.env(on, env)
  on('turn.start', (_$, e) => ({ turnId: e.turnId }))
  on('turn.complete', () => ({ text: 'done' }))
  on('process.run', async (_$, e) => {
    const call = { argv: e.argv, timeoutMs: e.init?.timeoutMs }
    calls.push(call)
    const { exitCode } = await run(call)
    return { value: { exitCode, stdout: '', stderr: '', isStdoutTruncated: false, isStderrTruncated: false } }
  })
  return calls
}

const argv = (program: string, event: string) => [
  program, 'accept', event, '--pane', '%7', '--adapter', 'claude.lifecycle',
]

test('a typed turn reports busy then idle', async ($, on) => {
  const calls = bottom(on, ENV, async () => ({ exitCode: 0 }))
  const INSTALLED = `${new URL('..', import.meta.url).pathname}../../harold`.replace(/\/+/g, '/')
  const started = await $.turn.start({ text: 'hi', turnId: 't1' })
  const done = await $.turn.complete({ ...COMPLETE, reason: 'answer' })

  expect(started).toEqual({ turnId: 't1' })
  expect(done).toEqual({ text: 'done' })
  expect(calls).toEqual([
    { argv: argv(INSTALLED, 'turn-started'), timeoutMs: 4000 },
    { argv: argv(INSTALLED, 'turn-completed'), timeoutMs: 4000 },
  ])
})

test('a subagent turn.complete reports nothing', async ($, on) => {
  const calls = bottom(on, ENV, async () => ({ exitCode: 0 }))
  await $.turn.complete({ ...COMPLETE, agentId: 'a1', reason: 'answer' })
  expect(calls).toEqual([])
})

test('a Rex block reports with REX_BLOCK as the pane', async ($, on) => {
  const calls = bottom(on, { REX_BLOCK: 'block:01a18fe6-c4f7-7b52-a9f2-9682b5c097c7' }, async () => ({ exitCode: 0 }))
  await $.turn.start({ text: 'hi', turnId: 't1' })
  expect(calls.length).toBe(1)
  expect(calls[0].argv.join(' ')).toContain('block:01a18fe6-c4f7-7b52-a9f2-9682b5c097c7')
})

test('a REX_BLOCK that is not a block id reports nothing', async ($, on) => {
  const calls = bottom(on, { REX_BLOCK: 'block:' }, async () => ({ exitCode: 0 }))
  await $.turn.start({ text: 'hi', turnId: 't1' })
  expect(calls).toEqual([])
})

test('no TMUX_PANE reports nothing', async ($, on) => {
  const calls = bottom(on, {}, async () => ({ exitCode: 0 }))
  await $.turn.start({ text: 'hi', turnId: 't1' })
  await $.turn.complete({ ...COMPLETE, reason: 'answer' })
  expect(calls).toEqual([])
})

test('a non-zero exit leaves the results unchanged', async ($, on) => {
  bottom(on, ENV, async () => ({ exitCode: 1 }))
  expect(await $.turn.start({ text: 'hi', turnId: 't1' })).toEqual({ turnId: 't1' })
  expect(await $.turn.complete({ ...COMPLETE, reason: 'answer' })).toEqual({ text: 'done' })
})

test('a rejecting process.run leaves the results unchanged', async ($, on) => {
  bottom(on, ENV, async () => { throw new Error('no harold') })
  expect(await $.turn.start({ text: 'hi', turnId: 't1' })).toEqual({ turnId: 't1' })
  expect(await $.turn.complete({ ...COMPLETE, reason: 'answer' })).toEqual({ text: 'done' })
})

test('reports finish in the order the events fired, even when the first is slow', async ($, on) => {
  const finished: string[] = []
  bottom(on, ENV, async c => {
    if (c.argv.includes('turn-started')) await new Promise(r => setTimeout(r, 50))
    finished.push(c.argv[2])
    return { exitCode: 0 }
  })
  await Promise.all([
    $.turn.start({ text: 'hi', turnId: 't1' }),
    $.turn.complete({ ...COMPLETE, reason: 'answer' }),
  ])
  expect(finished).toEqual(['turn-started', 'turn-completed'])
})

for (const reason of ['aborted', 'error'] as const) {
  test(`a turn that ends as ${reason} reports turn-completed`, async ($, on) => {
    const calls = bottom(on, ENV, async () => ({ exitCode: 0 }))
    await $.turn.complete({ ...COMPLETE, reason })
    expect(calls.map(c => c.argv[2])).toEqual(['turn-completed'])
  })
}

test('a turn that ends as a refusal reports turn-completed', async ($, on) => {
  const calls = bottom(on, ENV, async () => ({ exitCode: 0 }))
  await $.turn.complete({
    ...COMPLETE, reason: 'refusal', refusal: { category: null, explanation: null },
  })
  expect(calls.map(c => c.argv[2])).toEqual(['turn-completed'])
})

test('HAROLD_BIN names a different program', async ($, on) => {
  const calls = bottom(on, { ...ENV, HAROLD_BIN: '/x/harold' }, async () => ({ exitCode: 0 }))
  await $.turn.start({ text: 'hi', turnId: 't1' })
  expect(calls.map(c => c.argv)).toEqual([argv('/x/harold', 'turn-started')])
})

test('a turn.start that carries an agentId reports nothing', async ($, on) => {
  const calls = bottom(on, ENV, async () => ({ exitCode: 0 }))
  await $.turn.start({ text: 'hi', turnId: 't1', agentId: 'a1' } as never)
  expect(calls).toEqual([])
})

test('a turn.start with agentId undefined still reports', async ($, on) => {
  const calls = bottom(on, ENV, async () => ({ exitCode: 0 }))
  await $.turn.start({ text: 'hi', turnId: 't1', agentId: undefined } as never)
  expect(calls.map(c => c.argv[2])).toEqual(['turn-started'])
})

test('a pane id that is not % and digits reports nothing', async ($, on) => {
  const calls = bottom(on, { TMUX_PANE: '47' }, async () => ({ exitCode: 0 }))
  await $.turn.start({ text: 'hi', turnId: 't1' })
  await $.turn.complete({ ...COMPLETE, reason: 'answer' })
  expect(calls).toEqual([])
})

test('an event after a rejecting run still reports', async ($, on) => {
  let first = true
  const calls = bottom(on, ENV, async () => {
    if (first) {
      first = false
      throw new Error('no harold')
    }
    return { exitCode: 0 }
  })
  await $.turn.start({ text: 'hi', turnId: 't1' })
  await $.turn.complete({ ...COMPLETE, reason: 'answer' })
  expect(calls.map(c => c.argv[2])).toEqual(['turn-started', 'turn-completed'])
})

test('a report queued behind a slow failing one is skipped, so a turn waits about one call', async ($, on) => {
  const calls = bottom(on, ENV, async () => {
    await new Promise(r => setTimeout(r, 80))
    throw new Error('timed out')
  })
  const started = Date.now()
  const [start, done] = await Promise.all([
    $.turn.start({ text: 'hi', turnId: 't1' }),
    $.turn.complete({ ...COMPLETE, reason: 'answer' }),
  ])
  expect(start).toEqual({ turnId: 't1' })
  expect(done).toEqual({ text: 'done' })
  expect(calls.map(c => c.argv[2])).toEqual(['turn-started'])
  expect(Date.now() - started).toBeLessThan(150)
})

test('a report queued behind a slow successful one still goes out, in order', async ($, on) => {
  const calls = bottom(on, ENV, async c => {
    if (c.argv.includes('turn-started')) await new Promise(r => setTimeout(r, 40))
    return { exitCode: 0 }
  })
  await Promise.all([
    $.turn.start({ text: 'hi', turnId: 't1' }),
    $.turn.complete({ ...COMPLETE, reason: 'answer' }),
  ])
  expect(calls.map(c => c.argv[2])).toEqual(['turn-started', 'turn-completed'])
})
