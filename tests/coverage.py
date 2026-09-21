#!/usr/bin/env python3
"""Measure Rust unit, CLI-process and optional real-SSH coverage together.

Requires the matching Rust llvm-tools component (or --llvm-bin). Reports are
written under target/coverage; no default fwm configuration is used.
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys


def run(command, **kwargs):
    display = " ".join(map(str, command)) if len(command) < 20 else f"{Path(command[0]).name} {command[1]} ({len(command) - 2} arguments)"
    print("+ " + display, flush=True)
    return subprocess.run(command, check=True, **kwargs)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--llvm-bin", type=Path)
    parser.add_argument("--with-ssh", action="store_true")
    parser.add_argument("--offline", action="store_true")
    parser.add_argument("--report-only", action="store_true")
    parser.add_argument("--fail-under-lines", type=float, default=0)
    parser.add_argument("--fail-under-functions", type=float, default=0)
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    output = root / "target/coverage"
    output.mkdir(parents=True, exist_ok=True)
    if args.llvm_bin:
        llvm = args.llvm_bin
    else:
        sysroot = Path(subprocess.check_output(["rustc", "--print", "sysroot"], text=True).strip())
        host = next(line.split(": ", 1)[1] for line in subprocess.check_output(["rustc", "-vV"], text=True).splitlines() if line.startswith("host: "))
        llvm = sysroot / "lib/rustlib" / host / "bin"
    suffix = ".exe" if os.name == "nt" else ""
    cov, profdata = llvm / ("llvm-cov" + suffix), llvm / ("llvm-profdata" + suffix)
    if not cov.exists() or not profdata.exists():
        parser.error("install matching tools with rustup component add llvm-tools-preview, or pass --llvm-bin")
    raw = output / "raw"
    env = os.environ.copy()
    env["CARGO_INCREMENTAL"] = "0"
    env["CARGO_PROFILE_DEV_DEBUG"] = "1"
    env["CARGO_TARGET_DIR"] = str(output / "build")
    env["RUSTFLAGS"] = env.get("RUSTFLAGS", "") + " -C instrument-coverage"
    env["LLVM_PROFILE_FILE"] = str(raw / "fwm-%8m.profraw")
    manifest = output / "objects.json"
    if not args.report_only:
        if raw.exists():
            shutil.rmtree(raw)
        raw.mkdir()
        command = ["cargo", "test", "--workspace", "--all-targets", "--locked", "--no-fail-fast", "--message-format=json"]
        if args.offline:
            command.append("--offline")
        objects = set()
        print("+ " + " ".join(command), flush=True)
        process = subprocess.Popen(command, cwd=root, env=env, stdout=subprocess.PIPE, text=True)
        with (output / "tests.log").open("w") as log:
            for line in process.stdout:
                log.write(line)
                try:
                    event = json.loads(line)
                except json.JSONDecodeError:
                    if "test result:" in line or "FAILED" in line:
                        print(line.rstrip(), flush=True)
                    continue
                if event.get("reason") == "compiler-artifact" and event.get("executable"):
                    objects.add(event["executable"])
        if process.wait() != 0:
            raise SystemExit("Tests failed; see target/coverage/tests.log")
        binary = output / "build/debug" / ("fwm" + suffix)
        objects.add(str(binary))
        manifest.write_text(json.dumps(sorted(objects)))
        if args.with_ssh:
            run([sys.executable, str(root / "tests/smoke.py"), str(binary)], cwd=root, env=env)
    objects = json.loads(manifest.read_text())
    profiles = sorted(raw.glob("*.profraw"))
    if not profiles:
        raise SystemExit("No instrumented profiles were produced")
    profile_list = output / "profiles.txt"
    profile_list.write_text("\n".join(map(str, profiles)))
    merged = output / "coverage.profdata"
    run([str(profdata), "merge", "-sparse", "-f", str(profile_list), "-o", str(merged)])
    # Exclude external crates and standalone test sources. Inline Rust unit-test
    # code remains in the source metrics and is called out in TESTING.md.
    ignore = r"(/rustc/|/\.rustup/|/\.cargo/|/registry/|/tests/|/tests\.rs$|_tests\.rs$|_fixture\.rs$|/ssh/auth_agent\.rs$|/test_support\.rs$|/test_[^/]+\.rs$|/target/|/daemon/batch\.rs$)"
    common = [objects[0], "-instr-profile=" + str(merged), "-ignore-filename-regex=" + ignore]
    for path in objects[1:]:
        common.extend(["-object", path])
    with (output / "report.txt").open("w") as report:
        run([str(cov), "report", *common], stdout=report)
    with (output / "coverage.json").open("w") as report:
        run([str(cov), "export", *common], stdout=report)
    with (output / "lcov.info").open("w") as report:
        run([str(cov), "export", "-format=lcov", *common], stdout=report)
    run([str(cov), "show", *common, "-format=html", "-output-dir=" + str(output / "html")], stdout=subprocess.DEVNULL)
    totals = json.loads((output / "coverage.json").read_text())["data"][0]["totals"]
    print(json.dumps(totals, indent=2))
    print("Coverage report:", output / "html/index.html")
    if totals["lines"]["percent"] < args.fail_under_lines:
        raise SystemExit(f"Line coverage is below {args.fail_under_lines}%")
    if totals["functions"]["percent"] < args.fail_under_functions:
        raise SystemExit(f"Function coverage is below {args.fail_under_functions}%")


if __name__ == "__main__":
    main()
