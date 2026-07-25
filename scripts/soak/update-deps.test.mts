import assert from 'node:assert/strict'
import { test } from 'node:test'

import { selectEcosystems } from './update-deps.mts'

test('no ecosystem flag updates both', () => {
  assert.deepEqual(selectEcosystems([]), { npm: true, cargo: true })
  assert.deepEqual(selectEcosystems(['--dry-run']), { npm: true, cargo: true })
})

test('a single flag selects only that ecosystem', () => {
  assert.deepEqual(selectEcosystems(['--npm']), { npm: true, cargo: false })
  assert.deepEqual(selectEcosystems(['--cargo', '--dry-run']), { npm: false, cargo: true })
})

test('naming both explicitly means both, not neither (regression)', () => {
  assert.deepEqual(selectEcosystems(['--npm', '--cargo']), { npm: true, cargo: true })
})
