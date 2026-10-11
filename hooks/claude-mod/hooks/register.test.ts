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
  bottom(on, ENV, async () => { throw new Error('no grpcurl') })
  expect(await $.turn.start({ text: 'hi', turnId: 't1' })).toEqual({ turnId: 't1' })
  expect(await $.turn.complete({ ...COMPLETE, reason: 'answer' })).toEqual({ text: 'done' })
})

test('reports keep their order even when the first is slow', async ($, on) => {
  const calls = bottom(on, ENV, async c => {
    if (c.argv.includes('turn-started')) await new Promise(r => setTimeout(r, 50))
    return { exitCode: 0 }
  })
  await Promise.all([
    $.turn.start({ text: 'hi', turnId: 't1' }),
    $.turn.complete({ ...COMPLETE, reason: 'answer' }),
  ])
  expect(calls.map(c => c.argv[2])).toEqual(['turn-started', 'turn-completed'])
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
