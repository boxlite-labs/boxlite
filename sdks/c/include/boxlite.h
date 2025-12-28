#ifndef BOXLITE_H
#define BOXLITE_H

#pragma once

#include <stdarg.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>

/**
 * Opaque handle to a running box
 */
typedef struct CBoxHandle CBoxHandle;

/**
 * Opaque handle to a BoxliteRuntime instance
 */
typedef struct CBoxliteRuntime CBoxliteRuntime;

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

/**
 * Get BoxLite version string
 *
 * # Returns
 * Static string containing the version (e.g., "0.1.0")
 */
const char *boxlite_version(void);

/**
 * Create a new BoxLite runtime
 *
 * # Arguments
 * * `home_dir` - Path to BoxLite home directory (stores images, rootfs, etc.)
 *                If NULL, uses default: ~/.boxlite
 * * `out_error` - Output parameter for error message (caller must free with boxlite_free_string)
 *
 * # Returns
 * Pointer to CBoxliteRuntime on success, NULL on failure
 *
 * # Example
 * ```c
 * char *error = NULL;
 * BoxliteRuntime *runtime = boxlite_runtime_new("/tmp/boxlite", &error);
 * if (!runtime) {
 *     fprintf(stderr, "Error: %s\n", error);
 *     boxlite_free_string(error);
 *     return 1;
 * }
 * ```
 */
struct CBoxliteRuntime *boxlite_runtime_new(const char *home_dir, char **out_error);

/**
 * Create a new box with the given options (JSON)
 *
 * # Arguments
 * * `runtime` - BoxLite runtime instance
 * * `options_json` - JSON-encoded BoxOptions, e.g.:
 *                    `{"images": {"Reference": "alpine:3.19"}, "working_dir": "/workspace"}`
 * * `out_error` - Output parameter for error message
 *
 * # Returns
 * Pointer to CBoxHandle on success, NULL on failure
 *
 * # Example
 * ```c
 * const char *opts = "{\"images\":{\"Reference\":\"alpine:3.19\"}}";
 * BoxHandle *box = boxlite_create_box(runtime, image, opts, &error);
 * ```
 */
struct CBoxHandle *boxlite_create_box(struct CBoxliteRuntime *runtime,
                                      const char *image,
                                      const char *options_json,
                                      char **out_error);

/**
 * Execute a command in a box
 *
 * # Arguments
 * * `handle` - Box handle
 * * `command` - Command to execute
 * * `args_json` - JSON array of arguments, e.g.: `["arg1", "arg2"]`
 * * `callback` - Optional callback for streaming output (chunk_text, is_stderr, user_data)
 * * `user_data` - User data passed to callback
 * * `out_error` - Output parameter for error message
 *
 * # Returns
 * Exit code on success, -1 on failure
 *
 * # Example
 * ```c
 * const char *args = "[\"hello\"]";
 * int exit_code = boxlite_execute(box, "echo", args, NULL, NULL, &error);
 * ```
 */
int boxlite_execute(struct CBoxHandle *handle,
                    const char *command,
                    const char *args_json,
                    void (*callback)(const char*, int, void*),
                    void *user_data,
                    char **out_error);

/**
 * Stop a box
 *
 * # Arguments
 * * `handle` - Box handle (will be consumed/freed)
 * * `out_error` - Output parameter for error message
 *
 * # Returns
 * 0 on success, -1 on failure
 */
int boxlite_stop_box(struct CBoxHandle *handle, char **out_error);

/**
 * Free a runtime instance
 *
 * # Arguments
 * * `runtime` - Runtime instance to free (can be NULL)
 */
void boxlite_runtime_free(struct CBoxliteRuntime *runtime);

/**
 * Free a string allocated by BoxLite
 *
 * # Arguments
 * * `str` - String to free (can be NULL)
 */
void boxlite_free_string(char *str);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* BOXLITE_H */
