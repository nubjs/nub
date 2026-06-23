# aube publish smoke package

This package is intentionally tiny. The `publish-smoke` workflow copies it,
renames it to a throwaway npm package, stamps a unique prerelease version, and
publishes it to verify real registry behavior for `aube publish`.
