"""Prove native reflink placement before distributed acceptance starts.

    uv run --frozen python harness/v2/clone_out.py '{"work": "/mnt/btrfs"}'

One typed JSON object (inline, a file, or `-`) supplies an existing `work` directory on
APFS, btrfs or XFS with reflink; an empty work uses the system temporary directory.
The oracle measures a private file, a shared clone, then the blocks made private by
an edit. A copy backend fails even when it produces the same file bytes.

The native measurement and its causal checks are shared with distributed.py. The old
compiler build driver depended on the removed daemon/store protocol and is retired;
this oracle alone makes no claim about compiler build outputs or cache reuse.
"""

import ctypes
import os
import struct
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path[:0] = [str(HERE), str(HERE.parent)]
from typed_input import fail, parse

SCRIPT = "clone_out.py"


class Failed(Exception):
    """The filesystem cannot supply the required shared-block evidence."""


@dataclass(frozen=True)
class Input:
    work: str = ""


@dataclass(frozen=True)
class Tree:
    """A directory's regular files: their bytes, and the bytes shared with no other file."""

    files: int
    size: int
    private: int


# macOS: getattrlist(2).
ATTR_CMN_RETURNED_ATTRS = 0x8000_0000
ATTR_CMNEXT_PRIVATESIZE = 0x0000_0008
FSOPT_NOFOLLOW = 0x1
FSOPT_ATTR_CMN_EXTENDED = 0x20


class AttrList(ctypes.Structure):
    _fields_ = [("bitmapcount", ctypes.c_ushort), ("reserved", ctypes.c_uint16), ("commonattr", ctypes.c_uint32),
                ("volattr", ctypes.c_uint32), ("dirattr", ctypes.c_uint32), ("fileattr", ctypes.c_uint32),
                ("forkattr", ctypes.c_uint32)]


# Linux: linux/fiemap.h and linux/fs.h.
FS_IOC_FIEMAP = 0xC020_660B
FIEMAP_FLAG_SYNC = 0x1
FIEMAP_EXTENT_LAST = 0x1
FIEMAP_EXTENT_SHARED = 0x2000
FIEMAP_HEAD = struct.Struct("=QQIIII")
FIEMAP_EXTENT = struct.Struct("=QQQQQIIII")
EXTENTS_PER_CALL = 256
FICLONE = 0x4004_9409

LIBC = ctypes.CDLL(None, use_errno=True)
# The platform as a plain string, so a type check on any one platform reads every branch.
PLATFORM: str = sys.platform


def private_bytes(path: Path) -> int:
    """The bytes of `path` that share their blocks with no other file."""
    if PLATFORM == "darwin":
        asked = AttrList(5, 0, ATTR_CMN_RETURNED_ATTRS, 0, 0, 0, ATTR_CMNEXT_PRIVATESIZE)
        buf = ctypes.create_string_buffer(64)
        if LIBC.getattrlist(os.fsencode(path), ctypes.byref(asked), buf, len(buf),
                            FSOPT_NOFOLLOW | FSOPT_ATTR_CMN_EXTENDED) != 0:
            err = ctypes.get_errno()
            raise Failed(f"getattrlist(ATTR_CMNEXT_PRIVATESIZE) of {path}: {os.strerror(err)} (errno {err})")
        _, _, _, _, _, fork = struct.unpack_from("=I5I", buf, 0)
        if not fork & ATTR_CMNEXT_PRIVATESIZE:
            raise Failed(f"getattrlist of {path}: the volume does not say its private size (not APFS?)")
        return struct.unpack_from("=q", buf, 24)[0]
    if PLATFORM == "linux":
        import fcntl
        size = path.stat().st_size
        private, start = 0, 0
        with open(path, "rb") as f:
            while True:
                buf = bytearray(FIEMAP_HEAD.size + EXTENTS_PER_CALL * FIEMAP_EXTENT.size)
                FIEMAP_HEAD.pack_into(buf, 0, start, (1 << 64) - 1 - start, FIEMAP_FLAG_SYNC, 0, EXTENTS_PER_CALL, 0)
                try:
                    fcntl.ioctl(f.fileno(), FS_IOC_FIEMAP, buf)
                except OSError as e:
                    raise Failed(f"FIEMAP of {path}: {e}") from None
                mapped = FIEMAP_HEAD.unpack_from(buf, 0)[3]
                last = False
                for n in range(mapped):
                    logical, _, length, _, _, flags, _, _, _ = FIEMAP_EXTENT.unpack_from(buf, FIEMAP_HEAD.size + n * FIEMAP_EXTENT.size)
                    end = min(logical + length, size)
                    if not flags & FIEMAP_EXTENT_SHARED and end > logical:
                        private += end - logical
                    start = logical + length
                    last = last or bool(flags & FIEMAP_EXTENT_LAST)
                if last or mapped == 0:
                    return private
    raise Failed(f"no measure of shared blocks on {PLATFORM}")


def measure(root: Path) -> Tree:
    files = size = private = 0
    for top, _, names in os.walk(root):
        for name in names:
            path = Path(top) / name
            if path.is_symlink() or not path.is_file():
                continue
            files += 1
            size += path.stat().st_size
            private += private_bytes(path)
    return Tree(files, size, private)


def expect(holds: bool, what: str) -> None:
    if not holds:
        raise SystemExit(f"{SCRIPT} selftest: {what}")


def clone(source: Path, target: Path) -> None:
    if PLATFORM == "darwin":
        if LIBC.clonefile(os.fsencode(source), os.fsencode(target), 0) != 0:
            err = ctypes.get_errno()
            raise SystemExit(f"{SCRIPT} selftest: clonefile {source} to {target}: {os.strerror(err)}")
        return
    import fcntl
    with open(source, "rb") as src, open(target, "wb") as dst:
        try:
            fcntl.ioctl(dst.fileno(), FICLONE, src.fileno())
        except OSError as e:
            raise SystemExit(f"{SCRIPT} selftest: FICLONE {source} to {target}: {e} (no reflink on this volume?)") from None


def selftest(work: str = "") -> None:
    if PLATFORM not in ("darwin", "linux"):
        raise Failed(f"shared blocks are measured on macOS and Linux, not {PLATFORM}")
    size = 1 << 20
    with tempfile.TemporaryDirectory(dir=work or None) as tmp:
        a, b = Path(tmp) / "a", Path(tmp) / "b"
        a.write_bytes(os.urandom(size))
        with open(a, "rb+") as f:
            os.fsync(f.fileno())
        expect(private_bytes(a) == size, f"a fresh file is private: {private_bytes(a)} of {size}")
        clone(a, b)
        expect(private_bytes(a) == 0 and private_bytes(b) == 0,
               f"a clone shares every block: {private_bytes(a)}, {private_bytes(b)}")
        with open(b, "rb+") as f:
            f.write(os.urandom(8192))
            os.fsync(f.fileno())
        written = private_bytes(b)
        expect(0 < written < size // 4, f"a write into a clone makes only its blocks private: {written}")
        tree = measure(Path(tmp))
        expect(tree.files == 2 and tree.size == 2 * size, f"the tree is both files: {tree}")
    print(f"{SCRIPT} selftest: ok")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(f"usage: {SCRIPT} '<json>'\n\n{__doc__}")
    given = parse(Input, sys.argv[1], SCRIPT)
    try:
        selftest(given.work)
    except (Failed, OSError) as error:
        fail(SCRIPT, str(error))
