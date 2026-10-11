import { test, expect } from 'claude-code/testing'
import { isPaneId, buildRequest, buildInvocation, resolveConfig } from './report'

test('isPaneId accepts only % and digits', () => {
  expect(isPaneId('%4')).toBe(true)
  expect(isPaneId('%123')).toBe(true)
  for (const bad of ['', '47', '%4x', '%', 'x%4', undefined]) {
    expect(isPaneId(bad)).toBe(false)
  }
})

test('buildRequest has no workSummary', () => {
  const busy = buildRequest('%4', 'AGENT_STATE_BUSY')
  expect(busy).toEqual({ paneId: '%4', state: 'AGENT_STATE_BUSY', adapterId: 'claude.lifecycle' })
  expect('workSummary' in busy).toBe(false)
})

test('buildInvocation splits the proto path', () => {
  const request = buildRequest('%4', 'AGENT_STATE_IDLE')
  const inv = buildInvocation(request, { address: 'h:1', protoPath: '/a/b/harold.proto' })
  expect(inv.argv).toEqual([
    'grpcurl', '-plaintext', '-max-time', '3', '-import-path', '/a/b', '-proto', 'harold.proto',
    '-d', '@', 'h:1', 'harold.Harold/ReportAgentState',
  ])
  expect(JSON.parse(inv.stdin)).toEqual(request)
})

test('buildInvocation handles a proto path without a directory', () => {
  const inv = buildInvocation(buildRequest('%1', 'AGENT_STATE_IDLE'), { address: 'h:1', protoPath: 'harold.proto' })
  expect(inv.argv.slice(4, 8)).toEqual(['-import-path', '.', '-proto', 'harold.proto'])
})

test('resolveConfig trims and defaults', () => {
  expect(resolveConfig({ addr: undefined, proto: undefined, home: '/h' })).toEqual({
    address: 'localhost:50060', protoPath: '/h/bin/harold/harold.proto',
  })
  expect(resolveConfig({ addr: ' x:9 ', proto: ' /p/q.proto ', home: '/h' })).toEqual({
    address: 'x:9', protoPath: '/p/q.proto',
  })
  expect(resolveConfig({ addr: '  ', proto: '', home: '/h' }).address).toBe('localhost:50060')
})
