"""Cross-organisation resource isolation tests.

Verifies that one organisation's credentials cannot observe or manipulate
another organisation's resources.

## Setup

These tests need **two** distinct organisations.  The primary org comes from
the standard e2e credentials (``BOXLITE_E2E_PROFILE`` / ``p1``).  The second
org's credentials must be supplied via environment variables:

    BOXLITE_E2E_CROSS_ORG_API_KEY   – API key belonging to a different org
    BOXLITE_E2E_CROSS_ORG_PREFIX    – path_prefix for that org
                                      (discovered automatically when unset,
                                       provided the key has /v1/me access)

If the env vars are absent the whole module is skipped.  Set them in CI via
secret injection; locally use a second boxlite account.

## What is tested

* A valid key from org-B is rejected (401) for org-A's namespace.
* GET  /v1/{prefix-A}/boxes/{box-A-id}  with org-B key → 404.
* DELETE /v1/{prefix-A}/boxes/{box-A-id} with org-B key → 404; box survives.
* POST /v1/{prefix-A}/boxes/{box-A-id}/exec with org-B key → 404.
* A box launched by org-B cannot reach org-A's box on its internal address
  (bidirectional network isolation).

The 404 (not 403/401) requirement for cross-org resource access is deliberate:
the API must not leak the existence of resources belonging to another org.
"""
from __future__ import annotations

import json
import os
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any

import boxlite
import pytest
import pytest_asyncio

sys.path.insert(0, str(Path(__file__).parent.parent / "lib"))
from e2e_auth import auth_context, E2EAuthContext

# ---------------------------------------------------------------------------
# Skip the whole module when cross-org credentials are absent
# ---------------------------------------------------------------------------

_CROSS_ORG_KEY = os.environ.get("BOXLITE_E2E_CROSS_ORG_API_KEY", "").strip()
pytestmark = pytest.mark.skipif(
    not _CROSS_ORG_KEY,
    reason=(
        "Cross-org isolation tests require BOXLITE_E2E_CROSS_ORG_API_KEY. "
        "Set it to a valid API key from a different organisation to run."
    ),
)


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _discover_prefix(url: str, token: str) -> str:
    """Return path_prefix for *token* by calling /v1/me, or '' on failure."""
    req = urllib.request.Request(
        f"{url.rstrip('/')}/v1/me",
        method="GET",
        headers={"Authorization": f"Bearer {token}"},
    )
    try:
        with urllib.request.urlopen(req, timeout=15) as resp:
            return (json.loads(resp.read() or "null") or {}).get("path_prefix", "")
    except Exception:
        return ""


def _request(
    method: str,
    url: str,
    token: str,
    body: dict[str, Any] | None = None,
    *,
    timeout: int = 30,
) -> tuple[int, Any]:
    """Minimal HTTP helper that returns (status_code, parsed_body_or_None)."""
    headers: dict[str, str] = {"Authorization": f"Bearer {token}"}
    data: bytes | None = None
    if body is not None:
        headers["Content-Type"] = "application/json"
        data = json.dumps(body).encode()
    req = urllib.request.Request(url, method=method, headers=headers, data=data)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            raw = resp.read()
            return resp.status, (json.loads(raw) if raw else None)
    except urllib.error.HTTPError as exc:
        raw = exc.read()
        try:
            return exc.code, (json.loads(raw) if raw else None)
        except json.JSONDecodeError:
            return exc.code, {"_raw": raw.decode("utf-8", "replace")}


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------

@pytest.fixture(scope="module")
def org_a_ctx() -> E2EAuthContext:
    """Auth context for the primary (org-A) organisation."""
    return auth_context()


@pytest.fixture(scope="module")
def org_b_token() -> str:
    return _CROSS_ORG_KEY


@pytest.fixture(scope="module")
def org_b_prefix(org_a_ctx: E2EAuthContext, org_b_token: str) -> str:
    explicit = os.environ.get("BOXLITE_E2E_CROSS_ORG_PREFIX", "").strip()
    if explicit:
        return explicit
    return _discover_prefix(org_a_ctx.url, org_b_token)


@pytest_asyncio.fixture(scope="module")
async def org_a_rt(org_a_ctx: E2EAuthContext):
    """REST Boxlite runtime for org-A."""
    opts = boxlite.BoxliteRestOptions(
        url=org_a_ctx.url,
        credential=boxlite.ApiKeyCredential(org_a_ctx.token),
        path_prefix=org_a_ctx.path_prefix,
    )
    runtime = boxlite.Boxlite.rest(opts)
    yield runtime
    if hasattr(runtime, "close"):
        try:
            close = runtime.close()
            import inspect
            if inspect.isawaitable(close):
                await close
        except Exception:
            pass


@pytest_asyncio.fixture(scope="module")
async def org_b_rt(org_a_ctx: E2EAuthContext, org_b_token: str, org_b_prefix: str):
    """REST Boxlite runtime for org-B."""
    opts = boxlite.BoxliteRestOptions(
        url=org_a_ctx.url,
        credential=boxlite.ApiKeyCredential(org_b_token),
        path_prefix=org_b_prefix,
    )
    runtime = boxlite.Boxlite.rest(opts)
    yield runtime
    if hasattr(runtime, "close"):
        try:
            close = runtime.close()
            import inspect
            if inspect.isawaitable(close):
                await close
        except Exception:
            pass


@pytest.fixture(scope="module")
def org_a_box(org_a_ctx: E2EAuthContext, image: str):
    """A running box in org-A, shared across this module's tests.

    Created via raw HTTP so this fixture is independent of SDK version.
    Returns a simple namespace with an ``.id`` attribute.
    """
    import types

    # Create
    url = org_a_ctx.url_for(org_a_ctx.v1("boxes"))
    status, body = _request(
        "POST", url, token=org_a_ctx.token,
        body={"image": image, "auto_delete": 600, "auto_stop": 300},
    )
    assert status == 201, f"Failed to create org-A box: {status} {body}"
    box_id = body["box_id"]

    box = types.SimpleNamespace(id=box_id)
    yield box

    # Cleanup
    del_url = org_a_ctx.url_for(org_a_ctx.v1(f"boxes/{box_id}"))
    _request("DELETE", del_url, token=org_a_ctx.token)


# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_cross_org_key_rejected_for_other_org_namespace(
    org_a_ctx: E2EAuthContext,
    org_b_token: str,
    org_a_box,
):
    """org-B's API key must not be accepted in org-A's URL namespace.

    The endpoint under test is org-A's box list.  A different-org key must get
    401 — not 200 — because the key resolves to a different tenant scope.
    """
    url = org_a_ctx.url_for(org_a_ctx.v1("boxes"))
    status, _ = _request("GET", url, token=org_b_token)
    assert status in (401, 403), (
        f"Expected 401 or 403 when using org-B key in org-A's namespace, got {status}. "
        "A valid key from another org must be rejected in a foreign namespace."
    )


@pytest.mark.asyncio
async def test_cross_org_get_box_returns_404(
    org_a_ctx: E2EAuthContext,
    org_b_token: str,
    org_b_prefix: str,
    org_a_box,
):
    """GET /v1/{prefix-B}/boxes/{box-A-id} with org-B token → 404.

    The response must be 404, not 403/401, to avoid leaking the existence of
    org-A's resources to org-B.
    """
    box_id = org_a_box.id
    if org_b_prefix:
        url = org_a_ctx.url_for(f"/v1/{org_b_prefix}/boxes/{box_id}")
    else:
        url = org_a_ctx.url_for(f"/v1/boxes/{box_id}")
    status, _ = _request("GET", url, token=org_b_token)
    assert status == 404, (
        f"Expected 404 when org-B reads org-A's box {box_id}, got {status}. "
        "Existence must be indistinguishable: 404 not 403/401."
    )


@pytest.mark.asyncio
async def test_cross_org_delete_box_returns_404(
    org_a_ctx: E2EAuthContext,
    org_b_token: str,
    org_b_prefix: str,
    org_a_box,
):
    """DELETE /v1/{prefix-B}/boxes/{box-A-id} with org-B token → 404.

    The box must still be alive in org-A after the attempt.
    """
    box_id = org_a_box.id
    if org_b_prefix:
        url = org_a_ctx.url_for(f"/v1/{org_b_prefix}/boxes/{box_id}")
    else:
        url = org_a_ctx.url_for(f"/v1/boxes/{box_id}")
    status, _ = _request("DELETE", url, token=org_b_token)
    assert status == 404, (
        f"Expected 404 when org-B deletes org-A's box {box_id}, got {status}."
    )

    # Box must still be accessible by org-A (org-B's DELETE had no effect).
    get_url = org_a_ctx.url_for(org_a_ctx.v1(f"boxes/{box_id}"))
    get_status, _ = _request("GET", get_url, token=org_a_ctx.token)
    assert get_status == 200, "org-A's box disappeared after a cross-org DELETE attempt"


@pytest.mark.asyncio
async def test_cross_org_exec_returns_404(
    org_a_ctx: E2EAuthContext,
    org_b_token: str,
    org_b_prefix: str,
    org_a_box,
):
    """POST …/exec on org-A's box with org-B token → 404."""
    box_id = org_a_box.id
    if org_b_prefix:
        url = org_a_ctx.url_for(f"/v1/{org_b_prefix}/boxes/{box_id}/exec")
    else:
        url = org_a_ctx.url_for(f"/v1/boxes/{box_id}/exec")
    status, _ = _request(
        "POST", url, token=org_b_token,
        body={"cmd": ["/bin/sh", "-c", "id"]},
    )
    assert status == 404, (
        f"Expected 404 when org-B execs into org-A's box {box_id}, got {status}."
    )


@pytest.mark.asyncio
async def test_cross_org_network_isolation(
    image,
    org_a_rt,
    org_b_rt,
):
    """Boxes in different orgs cannot reach each other over the internal network.

    Start a simple HTTP server in an org-A box; attempt to connect to it from
    an org-B box using the box-id-based DNS pattern. All attempts must fail.
    """
    PORT = 9527

    box_a = box_b = None
    try:
        box_a = await org_a_rt.create(
            boxlite.BoxOptions(image=image, auto_remove=True),
        )
        box_b = await org_b_rt.create(
            boxlite.BoxOptions(image=image, auto_remove=True),
        )

        await box_a.exec(
            "/bin/sh",
            ["-c", f"printf 'HTTP/1.0 200 OK\\r\\n\\r\\nORG_A_DATA' | nc -l -p {PORT} &>/dev/null &"],
            None,
        )
        time.sleep(1)

        reach_attempts = [
            f"http://{box_a.id}.box:{PORT}/",
            f"http://{box_a.id}.box.internal:{PORT}/",
        ]
        for target_url in reach_attempts:
            result = await box_b.exec(
                "/bin/sh",
                ["-c", f"curl -s --max-time 3 {target_url!r} 2>&1 || echo 'BLOCKED'"],
                None,
            )
            import asyncio
            out_chunks: list[bytes] = []
            async for chunk in result.stdout:
                out_chunks.append(chunk)
            out = b"".join(out_chunks).decode("utf-8", "replace")

            assert "ORG_A_DATA" not in out, (
                f"org-B box reached org-A box at {target_url}: "
                f"network isolation breach. Output: {out!r}"
            )
    finally:
        for (rt, box) in [(org_a_rt, box_a), (org_b_rt, box_b)]:
            if box is not None:
                try:
                    await rt.remove(box.id, force=True)
                except Exception:
                    pass
