#!/usr/bin/env python3
"""Generate bounded-memory TAR fixtures; expanded bytes never touch disk."""

import argparse
import bz2
import gzip
import hashlib
import json
from pathlib import Path
import tarfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path)
    parser.add_argument("--large-mib", type=int, default=8704)
    parser.add_argument("--dense-mib", type=int, default=129)
    args = parser.parse_args()
    if not 128 <= args.large_mib < 20 * 1024 or not 128 <= args.dense_mib <= 512:
        parser.error("large size must be 128..20479 MiB; dense size 128..512 MiB")
    args.output.mkdir(parents=True, exist_ok=False)
    block_size = 1024 * 1024
    rows = []
    for kind, mib in [("zero-runs", args.large_mib), ("dense", args.dense_mib)]:
        size = mib * block_size + 113
        header = tarfile.TarInfo(kind + ".bin")
        header.size = size
        header.mode = 0o644
        path = args.output / (kind + ".tar.bz2")
        digest = hashlib.sha256()
        with bz2.open(path, "wb", compresslevel=1) as stream:
            stream.write(header.tobuf(format=tarfile.USTAR_FORMAT))
            for offset in range(0, size, block_size):
                count = min(block_size, size - offset)
                if kind == "dense":
                    data = hashlib.shake_256(b"archive-memory-fixture" + offset.to_bytes(8, "little")).digest(count)
                else:
                    data = bytearray(count)
                    # Payload on both sides of long zero runs, including the tail.
                    if offset in (0, size // 2 // block_size * block_size, size // block_size * block_size):
                        marker = b"retained-archive-evidence"
                        data[-len(marker):] = marker
                digest.update(data)
                stream.write(data)
            stream.write(bytes((-size) % 512 + 1024))
        with path.open("rb") as archive:
            archive_sha256 = hashlib.file_digest(archive, "sha256").hexdigest()
        rows.append({"path": path.name, "member": header.name, "logical_bytes": size,
                     "member_sha256": digest.hexdigest(), "archive_bytes": path.stat().st_size,
                     "archive_sha256": archive_sha256})

    # A valid gzip stream containing a TAR cut inside a large declared member.
    prefix = b"#!/bin/sh\necho readable-prefix\n"
    header = tarfile.TarInfo("payload.sh")
    header.size = 256 * block_size
    header.mode = 0o644
    path = args.output / "truncated-large.tar.gz"
    path.write_bytes(gzip.compress(header.tobuf(format=tarfile.USTAR_FORMAT) + prefix, mtime=0))
    rows.append({"path": path.name, "member": header.name, "declared_bytes": header.size,
                 "readable_bytes": len(prefix), "member_sha256": hashlib.sha256(prefix).hexdigest(),
                 "archive_bytes": path.stat().st_size,
                 "archive_sha256": hashlib.sha256(path.read_bytes()).hexdigest()})
    # Keep metadata outside the dataset so the worker scans only these archives.
    args.output.with_suffix(".manifest.json").write_text(json.dumps(rows, indent=2) + "\n")
    print(json.dumps(rows, indent=2))


if __name__ == "__main__":
    main()
