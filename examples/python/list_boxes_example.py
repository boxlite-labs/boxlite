#!/usr/bin/env python3
"""
List Boxes Example - Display all boxes and their status

Demonstrates how to list and inspect all boxes in the runtime.
"""

import boxlite


def main():
    """List all boxes with their information."""
    runtime = boxlite.Boxlite.default()

    # Get all boxes
    boxes = runtime.list_info()

    if not boxes:
        print("No boxes found.")
        return

    # Print header
    print(f"{'NAME':<20} {'ID':<28} {'STATE':<10} {'IMAGE':<20} {'CPU':<5} {'MEM':<8} {'PID':<8}")
    print("-" * 105)

    # Print each box
    for info in boxes:
        name_str = info.name if info.name else "-"
        pid_str = str(info.pid) if info.pid else "-"
        mem_str = f"{info.memory_mib}MB"
        # Truncate ID for display
        id_short = info.id[:12] + "..." if len(info.id) > 15 else info.id
        print(
            f"{name_str:<20} {id_short:<28} {info.state:<10} {info.image:<20} {info.cpus:<5} {mem_str:<8} {pid_str:<8}")

    print("-" * 105)
    print(f"Total: {len(boxes)} box(es)")


if __name__ == "__main__":
    main()
