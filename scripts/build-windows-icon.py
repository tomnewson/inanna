#!/usr/bin/env python3
"""Build a multi-resolution Windows ICO from a transparent PNG on macOS."""

from __future__ import annotations

import struct
import subprocess
import sys
import tempfile
from pathlib import Path


ICON_SIZES = (16, 20, 24, 32, 40, 48, 64, 128, 256)
PNG_SIGNATURE = b"\x89PNG\r\n\x1a\n"


def fail(message: str) -> None:
    raise SystemExit(message)


def main() -> None:
    if len(sys.argv) != 3:
        fail("Usage: build-windows-icon.py SOURCE.png OUTPUT.ico")
    if sys.platform != "darwin":
        fail("This script uses macOS sips and must run on macOS.")

    source = Path(sys.argv[1]).resolve()
    output = Path(sys.argv[2]).resolve()
    if not source.is_file():
        fail(f"Source PNG does not exist: {source}")

    images: list[tuple[int, bytes]] = []
    with tempfile.TemporaryDirectory(prefix="inanna-icon-") as temporary:
        temporary_path = Path(temporary)
        for size in ICON_SIZES:
            resized = temporary_path / f"icon-{size}.png"
            subprocess.run(
                ["sips", "-z", str(size), str(size), str(source), "--out", str(resized)],
                check=True,
                stdout=subprocess.DEVNULL,
            )
            data = resized.read_bytes()
            if not data.startswith(PNG_SIGNATURE) or len(data) < 24:
                fail(f"sips did not produce a valid PNG for {size}x{size}.")
            width, height = struct.unpack(">II", data[16:24])
            if (width, height) != (size, size):
                fail(f"Unexpected resized dimensions: {width}x{height}")
            images.append((size, data))

    header_size = 6 + (16 * len(images))
    offset = header_size
    entries = bytearray()
    payload = bytearray()
    for size, data in images:
        dimension = 0 if size == 256 else size
        entries.extend(
            struct.pack(
                "<BBBBHHII",
                dimension,
                dimension,
                0,
                0,
                1,
                32,
                len(data),
                offset,
            )
        )
        payload.extend(data)
        offset += len(data)

    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_bytes(struct.pack("<HHH", 0, 1, len(images)) + entries + payload)
    print(f"Built {output} from {source}")


if __name__ == "__main__":
    main()
