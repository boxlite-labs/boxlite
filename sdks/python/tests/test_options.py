"""
Tests for BoxOptions - auto_remove and detach options.

These tests verify the behavior of:
- auto_remove: Controls whether box is removed on stop()
- detach: Controls whether box is tied to parent process lifecycle
"""

from __future__ import annotations

import os
import sys
from pathlib import Path
from typing import Iterable

import boxlite
import pytest

pytestmark = pytest.mark.integration


def _candidate_library_dirs() -> Iterable[Path]:
    """Yield directories that may hold libkrun/libkrunfw dylibs."""
    package_dir = Path(boxlite.__file__).parent
    bundled = package_dir / ".dylibs"
    if bundled.exists():
        yield bundled

    # Homebrew layout on Apple Silicon
    hb_root = Path("/opt/homebrew/opt")
    hb_dirs = [hb_root / "libkrun" / "lib", hb_root / "libkrunfw" / "lib"]
    if all(path.exists() for path in hb_dirs):
        yield from hb_dirs


@pytest.fixture(autouse=True)
def _ensure_library_paths(monkeypatch):
    """Populate the dynamic loader search path so libkrun can be found."""
    dirs = [str(path) for path in _candidate_library_dirs()]
    if not dirs:
        pytest.skip("libkrun libraries are not available on this system")

    joined = ":".join(dirs)
    if sys.platform == "darwin":
        vars_to_set = ["DYLD_LIBRARY_PATH", "LD_LIBRARY_PATH"]
    else:
        vars_to_set = ["LD_LIBRARY_PATH"]

    for var in vars_to_set:
        existing = os.environ.get(var)
        value = joined if not existing else ":".join([joined, existing])
        monkeypatch.setenv(var, value)


@pytest.fixture
def runtime():
    """Create a runtime for testing."""
    rt = boxlite.Boxlite(boxlite.Options())
    try:
        yield rt
    finally:
        rt.close()


class TestBoxOptionsDefaults:
    """Test BoxOptions default values."""

    def test_auto_remove_default_is_none(self):
        """Test that auto_remove defaults to None (uses Rust default)."""
        opts = boxlite.BoxOptions()
        # Python side defaults to None, Rust side defaults to True
        assert opts.auto_remove is None

    def test_detach_default_is_none(self):
        """Test that detach defaults to None (uses Rust default)."""
        opts = boxlite.BoxOptions()
        # Python side defaults to None, Rust side defaults to False
        assert opts.detach is None

    def test_explicit_auto_remove_true(self):
        """Test setting auto_remove=True explicitly."""
        opts = boxlite.BoxOptions(image="alpine:latest", auto_remove=True)
        assert opts.auto_remove == True

    def test_explicit_auto_remove_false(self):
        """Test setting auto_remove=False explicitly."""
        opts = boxlite.BoxOptions(image="alpine:latest", auto_remove=False)
        assert opts.auto_remove == False

    def test_explicit_detach_true(self):
        """Test setting detach=True explicitly."""
        opts = boxlite.BoxOptions(image="alpine:latest", detach=True)
        assert opts.detach == True

    def test_explicit_detach_false(self):
        """Test setting detach=False explicitly."""
        opts = boxlite.BoxOptions(image="alpine:latest", detach=False)
        assert opts.detach == False


class TestAutoRemoveBehavior:
    """Test auto_remove option behavior."""

    def test_auto_remove_true_removes_box_on_stop(self, runtime):
        """Test that auto_remove=True removes box when stop() is called."""
        box = runtime.create(boxlite.BoxOptions(
            image="alpine:latest",
            auto_remove=True,
        ))
        box_id = box.id

        # Box should exist before stop
        assert runtime.get_info(box_id) is not None

        # Stop the box
        box.shutdown()

        # Box should be removed
        assert runtime.get_info(box_id) is None

    def test_auto_remove_false_preserves_box_on_stop(self, runtime):
        """Test that auto_remove=False preserves box when stop() is called."""
        box = runtime.create(boxlite.BoxOptions(
            image="alpine:latest",
            auto_remove=False,
        ))
        box_id = box.id

        # Stop the box
        box.shutdown()

        # Box should still exist
        info = runtime.get_info(box_id)
        assert info is not None
        assert info.state == "stopped"

        # Cleanup
        runtime.remove(box_id)


class TestDetachOption:
    """Test detach option is accepted."""

    def test_detach_false_creates_box(self, runtime):
        """Test that detach=False creates box successfully."""
        box = runtime.create(boxlite.BoxOptions(
            image="alpine:latest",
            detach=False,
            auto_remove=True,
        ))
        assert box is not None
        assert box.id is not None

        # Cleanup
        box.shutdown()

    def test_detach_true_creates_box(self, runtime):
        """Test that detach=True creates box successfully."""
        # Note: detach=True requires auto_remove=False (they are incompatible)
        box = runtime.create(boxlite.BoxOptions(
            image="alpine:latest",
            detach=True,
            auto_remove=False,
        ))
        assert box is not None
        assert box.id is not None

        # Cleanup
        box.shutdown()
        runtime.remove(box.id)


class TestInvalidCombinations:
    """Test that invalid option combinations are rejected."""

    def test_auto_remove_true_detach_true_rejected(self, runtime):
        """Test that auto_remove=True + detach=True is rejected."""
        with pytest.raises(RuntimeError) as exc_info:
            runtime.create(boxlite.BoxOptions(
                image="alpine:latest",
                auto_remove=True,
                detach=True,
            ))
        assert "incompatible" in str(exc_info.value).lower()


class TestCombinedOptions:
    """Test combinations of auto_remove and detach options."""

    def test_ephemeral_sandbox(self, runtime):
        """Test ephemeral sandbox: auto_remove=True, detach=False."""
        box = runtime.create(boxlite.BoxOptions(
            image="alpine:latest",
            auto_remove=True,
            detach=False,
        ))
        box_id = box.id

        # Box exists
        assert runtime.get_info(box_id) is not None

        # Stop - should auto-remove
        box.shutdown()

        # Box gone
        assert runtime.get_info(box_id) is None

    def test_persistent_sandbox(self, runtime):
        """Test persistent sandbox: auto_remove=False, detach=False."""
        box = runtime.create(boxlite.BoxOptions(
            image="alpine:latest",
            auto_remove=False,
            detach=False,
        ))
        box_id = box.id

        # Stop - should preserve
        box.shutdown()

        # Box still exists
        info = runtime.get_info(box_id)
        assert info is not None
        assert info.state == "stopped"

        # Can get new handle
        box2 = runtime.get(box_id)
        assert box2 is not None

        # Cleanup
        box2.shutdown()
        runtime.remove(box_id)

    def test_detached_service(self, runtime):
        """Test detached service: auto_remove=False, detach=True."""
        box = runtime.create(boxlite.BoxOptions(
            image="alpine:latest",
            auto_remove=False,
            detach=True,
        ))
        box_id = box.id

        # Box exists
        assert runtime.get_info(box_id) is not None

        # Stop
        box.shutdown()

        # Still exists (auto_remove=False)
        info = runtime.get_info(box_id)
        assert info is not None

        # Cleanup
        runtime.remove(box_id)


if __name__ == "__main__":
    pytest.main([__file__, "-v"])
