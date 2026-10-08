import { describe, it } from 'node:test'
import assert from 'node:assert/strict'
import { matchMalwareResults } from './check-safety.mts'

describe('matchMalwareResults', () => {
  it('matches by name@version, not index, so a dropped lookup is unverified not misattributed', () => {
    // The SDK compacts: a failed lookup is dropped, so data has 2 entries for
    // 3 specs. Index matching would shift every result after the drop; identity
    // matching lands each against the right spec.
    const specs = [
      { name: 'a', version: '1.0.0' },
      { name: 'b', version: '2.0.0' },
      { name: 'c', version: '3.0.0' },
    ]
    const data = [
      { name: 'a', version: '1.0.0', alerts: [] },
      { name: 'c', version: '3.0.0', alerts: [] },
    ]
    const out = matchMalwareResults(specs, data)
    assert.equal(out[0]!.verdict, 'ok')
    assert.equal(out[1]!.verdict, 'unverified')
    assert.equal(out[2]!.verdict, 'ok')
  })

  it('distinguishes two versions of one package (no name-only collision)', () => {
    const specs = [
      { name: 'is-positive', version: '1.0.0' },
      { name: 'is-positive', version: '3.1.0' },
    ]
    const data = [
      { name: 'is-positive', version: '1.0.0', alerts: [] },
      { name: 'is-positive', version: '3.1.0', alerts: [{ type: 'malware', key: 'x' }] },
    ]
    const out = matchMalwareResults(specs, data)
    assert.equal(out[0]!.verdict, 'ok')
    assert.equal(out[1]!.verdict, 'malware')
  })

  it('matches a scoped package via namespace/name@version', () => {
    const specs = [{ name: '@scope/pkg', version: '1.0.0' }]
    const data = [{ namespace: '@scope', name: 'pkg', version: '1.0.0', alerts: [] }]
    const out = matchMalwareResults(specs, data)
    assert.equal(out[0]!.verdict, 'ok')
  })

  it('reports malware when alerts are present', () => {
    const specs = [{ name: 'evil', version: '1.0.0' }]
    const data = [
      {
        name: 'evil',
        version: '1.0.0',
        alerts: [{ type: 'malware', key: 'npm-malware', severity: 'critical' }],
      },
    ]
    const out = matchMalwareResults(specs, data)
    assert.equal(out[0]!.verdict, 'malware')
    assert.equal(out[0]!.alerts.length, 1)
  })
})
