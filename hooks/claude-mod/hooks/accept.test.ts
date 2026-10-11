import { test, expect } from 'claude-code/testing'
import { isPaneId, resolveProgram, buildArgv } from './accept'

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
