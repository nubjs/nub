import assert from 'node:assert/strict'
import { test } from 'node:test'

import {
  ANNOTATION_RE,
  SOAK_DAYS,
  SOAK_MINUTES,
  VERSION_PIN_RE,
  addDaysIso,
  isValidIsoDate,
  todayIso,
} from './constants.mts'

test('SOAK_MINUTES derives from SOAK_DAYS', () => {
  assert.equal(SOAK_MINUTES, SOAK_DAYS * 24 * 60)
})

test('todayIso returns a YYYY-MM-DD string', () => {
  assert.match(todayIso(), /^\d{4}-\d{2}-\d{2}$/)
})

test('isValidIsoDate accepts real dates and rejects impossible ones', () => {
  assert.equal(isValidIsoDate('2026-07-11'), true)
  assert.equal(isValidIsoDate('2024-02-29'), true) // leap day
  assert.equal(isValidIsoDate('2026-13-45'), false) // shape-valid, not real
  assert.equal(isValidIsoDate('2026-02-30'), false)
  assert.equal(isValidIsoDate('not-a-date'), false)
})

test('addDaysIso rolls over months and years', () => {
  assert.equal(addDaysIso('2026-07-11', 7), '2026-07-18')
  assert.equal(addDaysIso('2026-12-28', 7), '2027-01-04')
  assert.equal(addDaysIso('2026-02-26', 7), '2026-03-05')
})

test('ANNOTATION_RE matches the published | removable comment shape', () => {
  assert.ok(ANNOTATION_RE.test('# published: 2026-07-08 | removable: 2026-07-15'))
  assert.ok(ANNOTATION_RE.test('#published: 2026-07-08|removable: 2026-07-15'))
  assert.ok(!ANNOTATION_RE.test('# published 2026-07-08 removable 2026-07-15'))
  assert.ok(!ANNOTATION_RE.test('- some-entry@1.2.3'))
})

test('VERSION_PIN_RE distinguishes dated pins from standing trust', () => {
  assert.ok(VERSION_PIN_RE.test('left-pad@1.3.0'))
  assert.ok(VERSION_PIN_RE.test('@scope/name@1.2.3'))
  assert.ok(!VERSION_PIN_RE.test('react')) // bare name
  assert.ok(!VERSION_PIN_RE.test('@myorg/*')) // scope glob
})
