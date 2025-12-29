/**
 * Simple BoxLite C API example
 *
 * Demonstrates creating a container and executing commands.
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "boxlite.h"

// Callback for streaming command output
void output_callback(const char* text, int is_stderr, void* user_data) {
    FILE* stream = is_stderr ? stderr : stdout;
    fprintf(stream, "%s", text);
}

int main() {
    char* error = NULL;

    printf("🚀 BoxLite C API Example\n");
    printf("Version: %s\n\n", boxlite_version());

    // Create runtime with default home directory
    CBoxliteRuntime* runtime = boxlite_runtime_new(NULL, &error);
    if (!runtime) {
        fprintf(stderr, "Failed to create runtime: %s\n", error);
        boxlite_free_string(error);
        return 1;
    }

    // Create a box with Alpine Linux
    const char* options_json = "{\"rootfs\":{\"Image\":\"alpine:3.19\"}}";
    CBoxHandle* box = boxlite_create_box(runtime, options_json, &error);
    if (!box) {
        fprintf(stderr, "Failed to create box: %s\n", error);
        boxlite_free_string(error);
        boxlite_runtime_free(runtime);
        return 1;
    }

    printf("📦 Created container, executing commands...\n\n");

    // Execute first command: list root directory
    printf("Command 1: ls -alrt /\n");
    printf("---\n");
    const char* args1 = "[\"-alrt\", \"/\"]";
    int exit_code = boxlite_execute(box, "/bin/ls", args1, output_callback, NULL, &error);
    if (exit_code != 0) {
        fprintf(stderr, "Command failed with exit code %d\n", exit_code);
        if (error) {
            fprintf(stderr, "Error: %s\n", error);
            boxlite_free_string(error);
        }
    }
    printf("\n");

    // Execute second command: show network interfaces
    printf("Command 2: ip addr\n");
    printf("---\n");
    const char* args2 = "[\"addr\"]";
    exit_code = boxlite_execute(box, "ip", args2, output_callback, NULL, &error);
    if (exit_code != 0) {
        fprintf(stderr, "Command failed with exit code %d\n", exit_code);
        if (error) {
            fprintf(stderr, "Error: %s\n", error);
            boxlite_free_string(error);
        }
    }
    printf("\n");

    // Execute third command: show environment
    printf("Command 3: env\n");
    printf("---\n");
    const char* args3 = "[]";
    exit_code = boxlite_execute(box, "/usr/bin/env", args3, output_callback, NULL, &error);
    if (exit_code != 0) {
        fprintf(stderr, "Command failed with exit code %d\n", exit_code);
        if (error) {
            fprintf(stderr, "Error: %s\n", error);
            boxlite_free_string(error);
        }
    }
    printf("\n");

    printf("✅ Execution completed!\n");

    // Cleanup
    if (boxlite_shutdown_box(box, &error) != 0) {
        fprintf(stderr, "Warning: Failed to shutdown box: %s\n", error);
        boxlite_free_string(error);
    }

    boxlite_runtime_free(runtime);

    return 0;
}
