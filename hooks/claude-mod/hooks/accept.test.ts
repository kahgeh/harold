import { test, expect } from 'claude-code/testing'
import { isPaneId, resolveProgram, buildArgv, hasWorking } from './accept'

test('isPaneId accepts only % and digits', () => {
  expect(isPaneId('%4')).toBe(true)
  expect(isPaneId('%123')).toBe(true)
  for (const bad of ['', '47', '%4x', '%', 'x%4', undefined]) {
    expect(isPaneId(bad)).toBe(false)
  }
})

test('resolveProgram prefers a trimmed HAROLD_BIN, else the installed binary', () => {
  const root = '/m/claude-mod'
  expect(resolveProgram({ bin: undefined, root })).toBe('/m/claude-mod/../../harold')
  expect(resolveProgram({ bin: ' /x/harold ', root })).toBe('/x/harold')
  expect(resolveProgram({ bin: '  ', root })).toBe('/m/claude-mod/../../harold')
})

test('buildArgv is the exact harold accept command', () => {
  expect(buildArgv({ program: '/p/harold', event: 'turn-started', paneId: '%47' })).toEqual([
    '/p/harold', 'accept', 'turn-started', '--pane', '%47', '--adapter', 'claude.lifecycle',
  ])
  expect(buildArgv({ program: 'h', event: 'turn-completed', paneId: '%1' })[2]).toBe('turn-completed')
})

test('buildArgv carries each of the four events', () => {
  for (const event of ['turn-started', 'still-working', 'turn-completed', 'subagents-finished'] as const) {
    expect(buildArgv({ program: 'h', event, paneId: '%1' })[2]).toBe(event)
  }
})

test('hasWorking counts pending, running and waiting, and nothing else', () => {
  for (const status of ['pending', 'running', 'waiting']) {
    expect(hasWorking([{ status: 'completed' }, { status }])).toBe(true)
  }
  expect(hasWorking(['idle', 'completed', 'failed', 'killed'].map(status => ({ status })))).toBe(false)
  expect(hasWorking([])).toBe(false)
})
