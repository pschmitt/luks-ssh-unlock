#!/usr/bin/env python3
"""Receive one client's bounded checksum archive through a forced SSH command."""

import fcntl
import io
import os
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile

MAX_ARCHIVE = 16 * 1024 * 1024
FILES = ("checksum", "checksum.sig", "meta.json")


def main():
    destination = Path(sys.argv[1])
    instance = sys.argv[2]
    archive = sys.stdin.buffer.read(MAX_ARCHIVE + 1)
    if len(archive) > MAX_ARCHIVE:
        raise ValueError("checksum archive exceeds the size limit")

    with tarfile.open(fileobj=io.BytesIO(archive), mode="r:") as tar:
        members = tar.getmembers()
        if len(members) != len(FILES) or {m.name for m in members} != set(FILES):
            raise ValueError("checksum archive must contain exactly the three expected files")
        if any(not m.isfile() or m.size == 0 for m in members):
            raise ValueError("checksum archive contains an invalid file")

        with tempfile.TemporaryDirectory(prefix=".push.", dir=destination) as temp:
            temp = Path(temp)
            for member in members:
                source = tar.extractfile(member)
                if source is None:
                    raise ValueError("cannot read checksum archive member")
                with source, (temp / member.name).open("xb") as output:
                    while chunk := source.read(65536):
                        output.write(chunk)

            # The controller's polling fetch locks the same parent directory.
            lock = os.open(destination.parent, os.O_RDONLY | os.O_DIRECTORY)
            try:
                fcntl.flock(lock, fcntl.LOCK_EX)
                (destination / "checksum.sig").unlink(missing_ok=True)
                for name in FILES:
                    os.replace(temp / name, destination / name)
            finally:
                os.close(lock)

    subprocess.run(
        ["@logger@", "-t", f"luks-ssh-unlock-push-{instance}", "Received signed initrd baseline"],
        check=False,
    )


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, tarfile.TarError) as error:
        print(f"luks-ssh-unlock: checksum receiver: {error}", file=sys.stderr)
        sys.exit(1)
