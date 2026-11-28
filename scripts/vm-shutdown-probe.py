#!/usr/bin/env python3
"""Verify a disposable VM's workspace and journal after guest poweroff."""

import argparse
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parent.parent


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("libkrun_source", type=Path, help="libkrun 1.19.7 source and release build")
    parser.add_argument("image_dir", type=Path, help="directory built by scripts/vm-image.sh")
    args = parser.parse_args()
    library = args.libkrun_source.resolve() / "target/release"

    with tempfile.TemporaryDirectory(prefix="deck-vm-shutdown-") as scratch:
        scratch = Path(scratch)
        binary = scratch / "probe"
        subprocess.run(
            ["cc", "-Wall", "-Wextra", "-Werror", "-I", str(args.libkrun_source / "include"),
             str(ROOT / "scripts/vm-exit-probe.c"), "-L", str(library), "-lkrun", "-o", str(binary)],
            check=True,
        )
        env = os.environ.copy()
        if sys.platform == "darwin":
            subprocess.run([str(ROOT / "scripts/vm-sign.sh"), str(binary)], check=True)
            (scratch / "libkrun.1.dylib").symlink_to(library / "libkrun.1.19.7.dylib")
            env["DYLD_LIBRARY_PATH"] = f"{scratch}:/opt/homebrew/opt/libkrunfw/lib:" + env.get("DYLD_LIBRARY_PATH", "")
        else:
            env["LD_LIBRARY_PATH"] = f"{library}:" + env.get("LD_LIBRARY_PATH", "")
        for name in ("base.img", "journal.img"):
            source = args.image_dir / name
            target = scratch / name
            if sys.platform == "darwin":
                subprocess.run(["cp", "-c", str(source), str(target)], check=True)
            else:
                shutil.copyfile(source, target)

        for mode in ("write", "verify"):
            console = scratch / f"{mode}.console"
            result = subprocess.run(
                [str(binary), str(scratch / "base.img"), str(console), str(scratch / "journal.img"),
                 "/usr/local/bin/deck-shutdown-probe", mode],
                env=env, capture_output=True, text=True, timeout=30,
            )
            output = console.read_text(errors="replace") if console.exists() else ""
            print(f"{mode}: exit {result.returncode}\n{result.stderr}{output}", flush=True)
            if result.returncode or "DECK_UNMOUNT_CONFIRMED" not in output:
                raise RuntimeError(f"{mode}: the guest did not confirm the unmount")
            if mode == "verify" and "DECK_DISKS_VERIFIED" not in output:
                raise RuntimeError("the guest did not verify both disks")
            if "reboot: Power down" not in output or "DECK_POWEROFF_FAILED" in output:
                raise RuntimeError(f"{mode}: the kernel did not confirm poweroff")


if __name__ == "__main__":
    main()
