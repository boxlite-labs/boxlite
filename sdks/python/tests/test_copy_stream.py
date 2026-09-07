"""
Streaming file copy: an archive moved in or out with no file on the host.

The path-based `copy_in`/`copy_out` pair is covered elsewhere; these tests are
about the stream API — bytes that never touch the host filesystem, and the
shape hint that lets the box extract them without staging.
"""

from __future__ import annotations

import io
import tarfile

import pytest

from boxlite import SimpleBox

pytestmark = pytest.mark.integration


def tar_of(name: str, content: bytes) -> bytes:
    """A one-file tar, built here so the test owns the bytes it sends."""
    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode="w") as archive:
        info = tarfile.TarInfo(name)
        info.size = len(content)
        info.mode = 0o644
        archive.addfile(info, io.BytesIO(content))
    return buffer.getvalue()


@pytest.mark.asyncio
async def test_streamed_archive_lands_in_the_box():
    async with SimpleBox(image="alpine:latest") as box:
        await box.copy_in_stream(
            "/root/streamed.txt",
            tar_of("streamed.txt", b"streamed in\n"),
            source_is_dir=False,
        )

        result = await box.exec("cat", "/root/streamed.txt")
        assert result.stdout == "streamed in\n"


@pytest.mark.asyncio
async def test_a_file_like_source_is_read_in_chunks():
    """`open(path, "rb")` must work as a source, not just bytes."""
    async with SimpleBox(image="alpine:latest") as box:
        payload = b"x" * (3 * 1024 * 1024)
        archive = io.BytesIO(tar_of("big.bin", payload))

        await box.copy_in_stream("/root/big.bin", archive, source_is_dir=False)

        result = await box.exec("wc", "-c", "/root/big.bin")
        assert result.stdout.split()[0] == str(len(payload))


@pytest.mark.asyncio
async def test_streamed_read_reports_the_shape_and_the_bytes():
    async with SimpleBox(image="alpine:latest") as box:
        await box.exec("sh", "-c", "printf 'out via stream\\n' > /root/out.txt")

        stream = await box.copy_out_stream("/root/out.txt")
        assert stream.source_is_dir is False

        archive = b"".join([chunk async for chunk in stream])

        # A tar, not the raw file: it unpacks, and the entry is what we asked
        # for. Reading it back with tarfile is what proves the stream carried
        # a whole archive rather than a truncated one.
        with tarfile.open(fileobj=io.BytesIO(archive)) as unpacked:
            names = unpacked.getnames()
            assert "out.txt" in names
            member = unpacked.extractfile("out.txt")
            assert member is not None
            assert member.read() == b"out via stream\n"


@pytest.mark.asyncio
async def test_a_directory_read_reports_itself_as_one():
    async with SimpleBox(image="alpine:latest") as box:
        await box.exec("sh", "-c", "mkdir -p /root/tree && touch /root/tree/a")

        stream = await box.copy_out_stream("/root/tree")
        assert stream.source_is_dir is True
        # Drain: an unread stream leaves the transfer open.
        async for _chunk in stream:
            pass


@pytest.mark.asyncio
async def test_a_source_that_fails_mid_archive_fails_the_copy():
    """
    The one way a streamed copy can silently do the wrong thing: a source that
    dies part-way leaves a tar cut on a block boundary, which extracts without
    error. The copy has to fail instead.
    """

    async def broken_source():
        yield b"\0" * 512  # a plausible header block
        raise RuntimeError("source went away")

    async with SimpleBox(image="alpine:latest") as box:
        with pytest.raises(RuntimeError, match="source went away"):
            await box.copy_in_stream(
                "/root/never.txt", broken_source(), source_is_dir=False
            )

        result = await box.exec("sh", "-c", "test -e /root/never.txt; echo $?")
        assert result.stdout.strip() == "1"


@pytest.mark.asyncio
async def test_a_refused_destination_reports_the_refusal_not_the_channel():
    """
    A destination the box refuses ends the upload while the caller is still
    writing. Past the write window the send fails, and the failure the caller
    sees must be the box's reason — not "the copy ended", which is what a
    closed channel says and tells nobody why.
    """
    async with SimpleBox(image="alpine:latest") as box:
        # /tmp is a mount inside the box, so the destination is refused
        # outright. The payload is far larger than the in-flight window, so
        # the writes outlive the refusal.
        payload = b"y" * (8 * 1024 * 1024)
        archive = io.BytesIO(tar_of("blocked.bin", payload))

        with pytest.raises(Exception, match="'/tmp' mount"):
            await box.copy_in_stream("/tmp/blocked", archive, source_is_dir=False)
