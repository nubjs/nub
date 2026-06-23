# Registry and auth

aube uses npm registry protocols and pnpm-compatible `.npmrc` configuration.

## Registries

```ini
registry=https://registry.npmjs.org/
@acme:registry=https://registry.example.test/
```

The global `--registry` flag overrides the default registry for one command:

```sh
aube --registry=https://registry.example.test install
```

Commands such as `publish`, `login`, `logout`, `deprecate`, `undeprecate`, and
`unpublish` also accept their own `--registry` flag.

## Tokens

```ini
//registry.npmjs.org/:_authToken=${NPM_TOKEN}
```

Log in interactively or with a pasted token:

```sh
aube login
aube login --scope @acme --registry https://registry.example.test/
aube logout --scope @acme
```

## Proxies and TLS

aube reads common npm proxy and TLS settings:

```ini
https-proxy=http://proxy.example.test:8080
noproxy=localhost,127.0.0.1
strict-ssl=true
cafile=/path/to/corp-ca.pem
```

## Cache tools

```sh
aube cache list
aube cache view react
aube cache delete '@babel/*'
aube cache list-registries
```

These commands inspect and prune the packument metadata cache.
