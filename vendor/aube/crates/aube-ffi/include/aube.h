#ifndef AUBE_H
#define AUBE_H

#include <stdint.h>

#if defined(_WIN32)
#define AUBE_API __declspec(dllimport)
#else
#define AUBE_API
#endif

#ifdef __cplusplus
extern "C" {
#endif

/* Called from aube-managed threads. The callback and ctx must remain valid and
 * thread-safe until aube_wait returns for the operation. event_json is borrowed
 * and valid only for the duration of the callback. The callback must return
 * normally: it must not throw, panic, or unwind across this C boundary, and it
 * must not call aube_wait for its active operation. */
typedef void (*aube_event_cb)(const char *event_json, void *ctx);

/* host_json: {"name":"host","version":"1.0.0","defaults":{...}}
 * Call before starting operations. The first operation seals standalone host
 * defaults when init has not run. Returns 0 on success, -1 for invalid input,
 * and -2 when a panic is caught. */
AUBE_API int32_t aube_init(const char *host_json);

/* options_json must contain projectDir. Returns immediately with an operation
 * handle. Inputs are copied before return. Boundary failures are represented
 * by a completed handle; call aube_wait to retrieve the structured error. */
AUBE_API uint64_t aube_install(
    const char *options_json,
    aube_event_cb callback,
    void *ctx);

/* packages_json is a JSON array of package specifier strings. Returns a
 * completed handle containing a structured error for boundary failures. */
AUBE_API uint64_t aube_add(
    const char *project_dir,
    const char *packages_json,
    const char *options_json,
    aube_event_cb callback,
    void *ctx);

/* Blocks until completion and consumes the handle. Returns owned UTF-8 JSON:
 * {"ok":true} or {"ok":false,"code":"ERR_AUBE_*","message":"..."}.
 * Returns NULL only if an internal result cannot be represented as a C string;
 * treat that as ERR_AUBE_FFI_RUNTIME. Free a non-null result with
 * aube_string_free. */
AUBE_API char *aube_wait(uint64_t handle);

/* Next buffered event for an operation started with bufferEvents: true, or
 * NULL when none is pending or the handle is unknown/consumed. Free the
 * returned string with aube_string_free. Events still buffered when
 * aube_wait returns are discarded with the handle. */
AUBE_API char *aube_events_next(uint64_t handle);

/* Returns 0 when cancellation was requested, -1 for an unknown handle, and -2
 * when a panic is caught. */
AUBE_API int32_t aube_cancel(uint64_t handle);

/* Frees any string returned by this library. Null is accepted. */
AUBE_API void aube_string_free(char *value);

#ifdef __cplusplus
}
#endif

#endif
