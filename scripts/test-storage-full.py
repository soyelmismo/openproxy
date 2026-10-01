#!/usr/bin/env python3
"""Run the opted-in SQLite ENOSPC test on a private, bounded tmpfs (Linux)."""
import argparse
import os
from pathlib import Path
import subprocess
import sys
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("test_binary", type=Path, help="compiled telemetry_operational test executable")
    args = parser.parse_args()
    binary = args.test_binary.resolve(strict=True)
    if os.readlink("/proc/self/ns/mnt") == os.readlink("/proc/1/ns/mnt"):
        parser.error("run inside unshare --mount --propagation private; host mounts are forbidden")
    with tempfile.TemporaryDirectory(prefix="enospc-fs-", dir="/tmp/opencode") as root:
        subprocess.run(
            ["mount", "-t", "tmpfs", "-o", "size=32m,mode=700,nosuid,nodev,noexec",
             "openproxy-operational-test", root], check=True,
        )
        try:
            env = dict(os.environ, OPENPROXY_ENOSPC_TEST_ROOT=root)
            result = subprocess.run(
                [str(binary), "--exact", "test_os_enospc_admission_and_recovery",
                 "--ignored", "--nocapture"], env=env, timeout=90,
            )
            return result.returncode
        finally:
            subprocess.run(["umount", root], check=True)


if __name__ == "__main__":
    sys.exit(main())
