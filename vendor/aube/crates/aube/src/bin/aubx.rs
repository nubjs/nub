// Multicall shim for `aube dlx`. The real program lives in `src/main.rs`;
// this stub only exists to give Cargo a unique path per `[[bin]]` target
// (Cargo warns when multiple bins share a source file). Dispatch happens
// at runtime via `rewrite_multicall_argv`, which keys off `argv[0]`.
include!("../main.rs");
