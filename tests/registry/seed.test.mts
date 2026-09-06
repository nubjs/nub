import { describe, it } from 'node:test'
import assert from 'node:assert/strict'
import { buildMinimalPackument } from './seed.mts'

describe('buildMinimalPackument', () => {
  it('preserves the npm time field for the seeded version', () => {
    const out = buildMinimalPackument({
      name: 'is-positive',
      version: '3.1.0',
      versionMeta: { name: 'is-positive', version: '3.1.0' },
      time: {
        created: '2015-06-02T12:03:51.069Z',
        modified: '2022-06-19T02:48:25.175Z',
        '3.1.0': '2016-01-11T14:34:31.892Z',
        '1.0.0': '2015-06-02T12:03:51.069Z',
      },
      registryUrl: 'http://localhost:4874/',
    })
    const time = out.time as Record<string, string>
    assert.equal(time['3.1.0'], '2016-01-11T14:34:31.892Z')
    assert.equal(time.created, '2015-06-02T12:03:51.069Z')
    // The unseeded version's timestamp is dropped.
    assert.equal(time['1.0.0'], undefined)
  })

  it('rewrites the tarball URL to point at the local registry', () => {
    const out = buildMinimalPackument({
      name: 'is-positive',
      version: '3.1.0',
      versionMeta: { dist: { tarball: 'https://registry.npmjs.org/is-positive/-/is-positive-3.1.0.tgz' } },
      registryUrl: 'http://localhost:4874/',
    })
    const versions = out.versions as Record<string, { dist: { tarball: string } }>
    assert.equal(
      versions['3.1.0']!.dist.tarball,
      'http://localhost:4874/is-positive/-/is-positive-3.1.0.tgz',
    )
  })

  it('handles a scoped package name in the tarball URL', () => {
    const out = buildMinimalPackument({
      name: '@scope/pkg',
      version: '1.0.0',
      versionMeta: {},
      registryUrl: 'http://localhost:4874/',
    })
    const versions = out.versions as Record<string, { dist: { tarball: string } }>
    assert.equal(
      versions['1.0.0']!.dist.tarball,
      'http://localhost:4874/@scope/pkg/-/@scope/pkg-1.0.0.tgz',
    )
  })
})
