import { test, expect, mock } from 'claude-code/testing'
import type { On } from 'claude-code'

type Call = { argv: readonly string[]; stdin?: string; timeoutMs?: number }

const ENV = { TMUX_PANE: '%7', HOME: '/home/u' }
const COMPLETE = { answer: '', durationMs: 1, isAborted: false, turnId: 't1' }

// The bottom of the test: what the engine would answer.
const bottom = (on: On, env: Record<string, string>, run: (c: Call) => Promise<{ exitCode: number }>) => {
  const calls: Call[] = []
  mock.env(on, env)
  on('turn.start', (_$, e) => ({ turnId: e.turnId }))
  on('turn.complete', () => ({ text: 'done' }))
  on('process.run', async (_$, e) => {
    const call = { argv: e.argv, stdin: e.init?.stdin, timeoutMs: e.init?.timeoutMs }
    calls.push(call)
    const { exitCode } = await run(call)
    return { value: { exitCode, stdout: '', stderr: '', isStdoutTruncated: false, isStderrTruncated: false } }
  })
  return calls
}

const ARGV = [
  'grpcurl', '-plaintext', '-max-time', '3',
  '-import-path', '/home/u/bin/harold', '-proto', 'harold.proto',
  '-d', '@', 'localhost:50060', 'harold.Harold/ReportAgentState',
]
const stdinFor = (state: string) =>
  JSON.stringify({ paneId: '%7', state, adapterId: 'claude.lifecycle' })

test('a typed turn reports busy then idle', async ($, on) => {
  const calls = bottom(on, ENV, async () => ({ exitCode: 0 }))
  const started = await $.turn.start({ text: 'hi', turnId: 't1' })
  const done = await $.turn.complete({ ...COMPLETE, reason: 'answer' })

  expect(started).toEqual({ turnId: 't1' })
  expect(done).toEqual({ text: 'done' })
  expect(calls).toEqual([
    { argv: ARGV, stdin: stdinFor('AGENT_STATE_BUSY'), timeoutMs: 4000 },
    { argv: ARGV, stdin: stdinFor('AGENT_STATE_IDLE'), timeoutMs: 4000 },
  ])
})

test('a subagent turn.complete reports nothing', async ($, on) => {
  const calls = bottom(on, ENV, async () => ({ exitCode: 0 }))
  await $.turn.complete({ ...COMPLETE, agentId: 'a1', reason: 'answer' })
  expect(calls).toEqual([])
})

test('no TMUX_PANE reports nothing', async ($, on) => {
  const calls = bottom(on, { HOME: '/home/u' }, async () => ({ exitCode: 0 }))
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
    if (c.stdin?.includes('BUSY')) await new Promise(r => setTimeout(r, 50))
    return { exitCode: 0 }
  })
  await Promise.all([
    $.turn.start({ text: 'hi', turnId: 't1' }),
    $.turn.complete({ ...COMPLETE, reason: 'answer' }),
  ])
  expect(calls.map(c => JSON.parse(c.stdin ?? '{}').state)).toEqual([
    'AGENT_STATE_BUSY', 'AGENT_STATE_IDLE',
  ])
})
