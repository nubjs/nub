# Workspaces

aube discovers workspaces from `aube-workspace.yaml` and links workspace
packages into the isolated dependency graph. Existing pnpm projects can keep
`pnpm-workspace.yaml` during migration; aube reads it as the source for the
aube-owned manifest.

```yaml
packages:
  - "packages/*"
  - "apps/*"
```

## Workspace protocol

```json
{
  "dependencies": {
    "@acme/ui": "workspace:*",
    "@acme/config": "workspace:^"
  }
}
```

Workspace dependencies are linked to local packages during development and
converted to concrete versions for publishing/deploy flows.

## Filters

```sh
aube -F api run build
aube -F '@acme/*' test
aube -F './apps/web' install
aube -F 'api...' run build
aube -F '...web' run test
aube -F '!legacy' -r run lint
```

Supported selector forms:

- Exact package names.
- Globs such as `@acme/*`.
- Paths such as `./apps/web`.
- Dependency graph selectors such as `api...` and `api^...`.
- Dependent graph selectors such as `...web` and `...^web`.
- Git-ref selectors such as `[origin/main]`.
- Exclusions such as `!legacy`.

## Recursive mode

```sh
aube -r run build
aube -r list --depth 0
```

`-r` runs over every workspace package unless an explicit filter is present.

## Catalogs

aube resolves `catalog:` and `catalog:<name>` from `aube-workspace.yaml`.

```yaml
catalog:
  react: ^19.0.0
catalogs:
  test:
    vitest: ^3.0.0
```

```json
{
  "dependencies": {
    "react": "catalog:",
    "vitest": "catalog:test"
  }
}
```

## Deploy

```sh
aube -F api deploy dist/api
```

`deploy` copies the selected workspace package's publishable files, rewrites
workspace dependencies to concrete versions, and installs dependencies in the
target directory.
