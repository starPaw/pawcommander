#!/usr/bin/env python3
"""Run the repository's Rust checks and optionally record their timings."""

import argparse
import datetime
import json
import os
from pathlib import Path
import platform
import subprocess
import sys
import tempfile
import time


ROOT = Path(__file__).resolve().parent.parent


def utc_now():
    return datetime.datetime.now(datetime.timezone.utc).isoformat()


def commands(quick, offline):
    cargo_flags = ["--locked"] + (["--offline"] if offline else [])
    stages = [("format", ["cargo", "fmt", "--check"])]
    if quick:
        stages.append(("check", ["cargo", "check", "--all-targets"] + cargo_flags))
    else:
        stages.extend(
            [
                ("test", ["cargo", "test"] + cargo_flags),
                (
                    "clippy",
                    ["cargo", "clippy", "--all-targets"]
                    + cargo_flags
                    + ["--", "-D", "warnings"],
                ),
            ]
        )
    stages.append(("diff", ["git", "diff", "--check"]))
    return stages


def tool_versions():
    versions = {"python": platform.python_version()}
    for tool in ("rustc", "cargo", "git"):
        try:
            result = subprocess.run(
                [tool, "--version"],
                cwd=ROOT,
                capture_output=True,
                text=True,
                check=False,
            )
            versions[tool] = {
                "output": (result.stdout or result.stderr).strip(),
                "exit_code": result.returncode,
            }
        except OSError as error:
            versions[tool] = {"error": str(error), "exit_code": None}
    return versions


def write_report(path, report):
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(
            mode="w", encoding="utf-8", dir=path.parent, delete=False
        ) as handle:
            temporary = Path(handle.name)
            json.dump(report, handle, indent=2, ensure_ascii=False)
            handle.write("\n")
        os.replace(temporary, path)
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--offline", action="store_true", help="use cached Cargo dependencies only"
    )
    parser.add_argument(
        "--quick",
        action="store_true",
        help="run formatting, cargo check and diff checks; does not run tests or Clippy",
    )
    parser.add_argument(
        "--report",
        type=Path,
        metavar="PATH",
        help="write a JSON timing report, including failures (relative to repository root)",
    )
    args = parser.parse_args(argv)
    started = time.monotonic()
    report = {
        "schema_version": 1,
        "started_utc": utc_now(),
        "platform": platform.platform(),
        "machine": platform.machine(),
        "mode": "quick" if args.quick else "full",
        "offline": args.offline,
        "tool_versions": {},
        "stages": [],
    }
    exit_code = 0
    try:
        report["tool_versions"] = tool_versions()
        for name, command in commands(args.quick, args.offline):
            print("[{}] {}".format(name, " ".join(command)), flush=True)
            stage_started = time.monotonic()
            stage = {"name": name, "command": command}
            try:
                result = subprocess.run(command, cwd=ROOT, check=False)
                stage["exit_code"] = result.returncode
            except OSError as error:
                stage["exit_code"] = 127 if isinstance(error, FileNotFoundError) else 126
                stage["error"] = str(error)
                print(str(error), file=sys.stderr, flush=True)
            except KeyboardInterrupt:
                stage["exit_code"] = 130
                stage["error"] = "interrupted"
            stage["elapsed_seconds"] = time.monotonic() - stage_started
            report["stages"].append(stage)
            print(
                "[{}] exit {} in {:.2f}s".format(
                    name, stage["exit_code"], stage["elapsed_seconds"]
                ),
                flush=True,
            )
            if stage["exit_code"] != 0:
                code = stage["exit_code"]
                exit_code = code if code > 0 else 128 - code
                break
    except KeyboardInterrupt:
        exit_code = 130
    finally:
        report["finished_utc"] = utc_now()
        report["elapsed_seconds"] = time.monotonic() - started
        report["exit_code"] = exit_code
        if args.report is not None:
            path = args.report.expanduser()
            if not path.is_absolute():
                path = ROOT / path
            try:
                write_report(path, report)
                print("Report: {}".format(path), flush=True)
            except OSError as error:
                print("Could not write report: {}".format(error), file=sys.stderr)
                exit_code = exit_code or 1
        print("Total: {:.2f}s; exit {}".format(report["elapsed_seconds"], exit_code))
    return exit_code


if __name__ == "__main__":
    sys.exit(main())
