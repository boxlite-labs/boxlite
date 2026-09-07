"""E2E port of `src/boxlite/tests/execution_shutdown.rs`.

Verifies the behaviour of exec and box state during/after box.stop():
pending exec.wait() should resolve, and a new exec attempt against a
stopped box auto-starts it (intended behaviour, see PR #861) rather
than being rejected.
"""
from __future__ import annotations

import asyncio

import boxlite
import pytest

from conftest import drain


@pytest.mark.asyncio
async def test_wait_resolves_after_box_stop(rt, image):
    box = await rt.create(boxlite.BoxOptions(image=image, auto_remove=False))
    try:
        ex = await box.exec("sh", ["-c", "sleep 60"], None)
        # Stop the box while exec is still running. wait() should resolve
        # (with whatever exit code the runtime reports) within a few
        # seconds, not hang.
        await asyncio.sleep(0.5)
        await box.stop()
        try:
            rc = await asyncio.wait_for(ex.wait(), timeout=30)
            # Whatever exit code is fine; the point is it resolved.
            assert rc is not None
        except asyncio.TimeoutError:
            pytest.fail("ex.wait() did not resolve within 30s after box.stop()")
    finally:
        try:
            await rt.remove(box.id, force=True)
        except Exception:
            pass


@pytest.mark.asyncio
async def test_exec_on_stopped_box_auto_starts_it(rt, image):
    """exec on a stopped box auto-starts it (see PR #861) — the exec
    itself must succeed, and the box's reported status must converge
    to running rather than staying stuck at stopped."""
    box = await rt.create(boxlite.BoxOptions(image=image, auto_remove=False))
    try:
        await box.stop()

        ex = await box.exec("sh", ["-c", "echo woke"], None)
        out, _ = await drain(ex)
        rc = await asyncio.wait_for(ex.wait(), timeout=30)
        assert rc.exit_code == 0, f"exec on stopped box failed: rc={rc.exit_code}"
        assert "woke" in out, f"exec on stopped box produced no output: {out!r}"

        # The auto-start is async on the server side; poll until the
        # reported status converges instead of asserting immediately.
        # `rt.get()` returns a `Box` handle (no `status`); the metadata
        # snapshot with status lives on `BoxInfo.state.status` via
        # `rt.get_info()`.
        for _ in range(10):
            info = await rt.get_info(box.id)
            if info is not None and str(info.state.status).lower() == "running":
                break
            await asyncio.sleep(1)
        else:
            pytest.fail(
                f"box status did not converge to running after exec "
                f"auto-started it: last status={info.state.status if info else None!r}"
            )
    finally:
        try:
            await rt.remove(box.id, force=True)
        except Exception:
            pass
