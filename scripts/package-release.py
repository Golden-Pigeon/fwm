#!/usr/bin/env python3
"""Package a built executable with completion support and its license materials."""
import argparse
from pathlib import Path
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--target", required=True)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--licenses", type=Path, required=True)
    parser.add_argument("--out", type=Path, default=ROOT / "dist")
    args = parser.parse_args()
    version = subprocess.check_output([str(args.binary.resolve()), "--version"], text=True).strip()
    args.out.mkdir(parents=True, exist_ok=True)
    name = "fwm-" + args.target
    with tempfile.TemporaryDirectory(prefix="fwm-release-") as temporary:
        package = Path(temporary) / name
        package.mkdir()
        executable = "fwm.exe" if "windows" in args.target else "fwm"
        shutil.copy2(args.binary, package / executable)
        (package / executable).chmod(0o755)
        shutil.copy2(ROOT / "LICENSE", package / "LICENSE")
        shutil.copy2(ROOT / "crates/fwm/THIRD_PARTY_NOTICES.txt", package / "THIRD_PARTY_NOTICES.txt")
        shutil.copy2(args.licenses / "DEPENDENCY_LICENSES.txt", package / "DEPENDENCY_LICENSES.txt")
        shutil.copytree(args.licenses / "dependency-sources", package / "dependency-sources")
        (package / "scripts").mkdir()
        shutil.copy2(ROOT / "scripts/install-shell-completions.sh", package / "scripts/install-shell-completions.sh")
        (package / "README.txt").write_text(
            f"{version} ({args.target})\n\n"
            "Run fwm --help to get started.\n"
            "Installation: https://github.com/Golden-Pigeon/fwm#install\n"
            "Source: https://github.com/Golden-Pigeon/fwm\n\n"
            "Keep LICENSE, THIRD_PARTY_NOTICES.txt, DEPENDENCY_LICENSES.txt, and\n"
            "dependency-sources with redistributions of this package.\n",
            encoding="utf-8",
        )
        archive_format = "zip" if "windows" in args.target else "gztar"
        archive = shutil.make_archive(str(args.out / name), archive_format, temporary, name)
        print(archive)


if __name__ == "__main__":
    main()
