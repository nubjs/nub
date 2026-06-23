# Publishing

aube implements the npm registry publish flow for package tarballs, dist-tags,
deprecations, and unpublishing.

## Pack

```sh
aube pack
aube pack --dry-run
aube pack --json
aube pack --pack-destination dist
```

`pack` applies npm-style file selection: `files` field first, otherwise
standard ignore rules, with `package.json`, README, LICENSE, and the `main`
entry always included.

## Publish

```sh
aube publish
aube publish --tag next
aube publish --access public
aube publish --dry-run --json
```

Workspace fanout uses the global workspace selectors:

```sh
aube -r publish
aube -F '@acme/*' publish
```

## Provenance

```sh
aube publish --provenance
```

Provenance requires an OIDC-capable CI environment such as GitHub Actions with
`id-token: write`. aube signs a SLSA in-toto statement via Sigstore and
attaches the bundle to the publish body.

## Dist-tags

```sh
aube dist-tag add react@18.2.0 stable
aube dist-tag ls react
aube dist-tag rm react stable
```

## Deprecate and unpublish

```sh
aube deprecate pkg@'<2' "Use pkg 2 or newer"
aube undeprecate pkg@'<2'
aube unpublish pkg@1.0.0 --dry-run
aube unpublish pkg --force
```

Whole-package unpublish requires `--force`.

