#!/usr/bin/env python3
"""Benchmark historypwd time-to-first-output-line and compare output compatibility.

The script creates deterministic temporary pwdlog fixtures, compares a preserved
baseline binary against the current optimized binary byte-for-byte, and measures
spawn-to-first-stdout-newline latency over repeated samples.
"""
from __future__ import annotations

import argparse
import json
import os
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable


@dataclass(frozen=True)
class Case:
    name: str
    kind: str
    args: tuple[str, ...]
    env: dict[str, str]
    pwdlog: Path
    benchmark: bool = True


@dataclass(frozen=True)
class Fixture:
    root: Path
    cases: tuple[Case, ...]


def write_pwdlog(path: Path, rows: Iterable[tuple[str, Path, str]]) -> None:
    with path.open("w", encoding="utf-8", newline="\n") as f:
        for stamp, cwd, command in rows:
            f.write(f"{stamp}\t{cwd}\t{command}\n")


def make_fixtures(root: Path) -> Fixture:
    home = root / "home"
    pwd = root / "cwd"
    home.mkdir(parents=True)
    pwd.mkdir(parents=True)

    # Shared filesystem shape.
    for dirname in [
        "alpha",
        "beta",
        "prefix-one",
        "prefix-two",
        "post/child",
        "dupe",
        "color-dir",
    ]:
        (pwd / dirname).mkdir(parents=True, exist_ok=True)
    (pwd / "file.txt").write_text("file\n", encoding="utf-8")
    (pwd / "script.sh").write_text("#!/bin/sh\n", encoding="utf-8")

    symlink_env: dict[str, str] = {}
    try:
        (pwd / "real-dir").mkdir(exist_ok=True)
        os.symlink("real-dir", pwd / "link-dir")
        os.symlink("missing-target", pwd / "dangling-link")
    except (OSError, NotImplementedError):
        symlink_env["HISTORYPWD_NO_SYMLINK_FIXTURE"] = "1"

    long_tail = " ".join(f"missing-token-{i}" for i in range(12000))
    fast_pwdlog = root / "pwdlog-fast"
    write_pwdlog(
        fast_pwdlog,
        [
            ("1", pwd, f"vim alpha {long_tail}"),
        ],
    )

    delayed_pwdlog = root / "pwdlog-delayed"
    delayed_rows: list[tuple[str, Path, str]] = [("1", pwd, "vim beta")]
    delayed_rows.extend((str(i + 2), pwd, f"echo missing-delayed-{i}") for i in range(1500))
    write_pwdlog(delayed_pwdlog, delayed_rows)

    duplicate_pwdlog = root / "pwdlog-duplicate"
    write_pwdlog(
        duplicate_pwdlog,
        [
            ("1", pwd, "vim prefix-two"),
            ("2", pwd, "vim dupe prefix-one"),
            ("3", pwd, "vim dupe"),
            ("4", pwd, "vim dupe"),
        ],
    )

    color_pwdlog = root / "pwdlog-color"
    write_pwdlog(color_pwdlog, [("1", pwd, "vim color-dir file.txt script.sh")])

    cd_pwdlog = root / "pwdlog-cd"
    write_pwdlog(cd_pwdlog, [("1", pwd, "cd post && vim child")])

    symlink_pwdlog = root / "pwdlog-symlink"
    write_pwdlog(symlink_pwdlog, [("1", pwd, "vim link-dir dangling-link missing-path")])

    common_env = {
        "HOME": str(home),
        "PWD": str(pwd),
        "FZF_HISTORY_COMPLETION_LINES": "5000",
        "FZF_HISTORY_COMPLETION_MAX_CANDIDATES": "3000",
    }

    cases = [
        Case(
            name="fast-hit-long-command",
            kind="dir",
            args=(".", "", "."),
            env={**common_env, "ZSH_PWD_HISTORY_FILE": str(fast_pwdlog), "FZF_HISTORY_COMPLETION_MAX_CANDIDATES": "1"},
            pwdlog=fast_pwdlog,
        ),
        Case(
            name="delayed-hit-many-lines",
            kind="dir",
            args=(".", "", "."),
            env={**common_env, "ZSH_PWD_HISTORY_FILE": str(delayed_pwdlog), "FZF_HISTORY_COMPLETION_MAX_CANDIDATES": "1"},
            pwdlog=delayed_pwdlog,
        ),
        Case(
            name="duplicate-leftover-prefix",
            kind="dir",
            args=(".", "pre", "."),
            env={**common_env, "ZSH_PWD_HISTORY_FILE": str(duplicate_pwdlog)},
            pwdlog=duplicate_pwdlog,
        ),
        Case(
            name="dir-and-file-paths",
            kind="path",
            args=(".", "", "."),
            env={**common_env, "ZSH_PWD_HISTORY_FILE": str(color_pwdlog)},
            pwdlog=color_pwdlog,
        ),
        Case(
            name="color-output",
            kind="path",
            args=("--color", ".", "", "."),
            env={**common_env, "ZSH_PWD_HISTORY_FILE": str(color_pwdlog), "LS_COLORS": "di=35:*.txt=32:ex=31"},
            pwdlog=color_pwdlog,
        ),
        Case(
            name="cd-post-cwd",
            kind="dir",
            args=(".", "", "."),
            env={**common_env, "ZSH_PWD_HISTORY_FILE": str(cd_pwdlog)},
            pwdlog=cd_pwdlog,
        ),
    ]
    if not symlink_env:
        cases.append(
            Case(
                name="symlink-and-dangling",
                kind="path",
                args=("--color", ".", "", "."),
                env={**common_env, "ZSH_PWD_HISTORY_FILE": str(symlink_pwdlog), "LS_COLORS": "di=35:ln=36:or=31"},
                pwdlog=symlink_pwdlog,
            )
        )
    return Fixture(root=root, cases=tuple(cases))


def command_for(binary: Path, case: Case) -> list[str]:
    return [str(binary), *case.args[:1], case.kind, *case.args[1:]] if case.args and case.args[0] == "--color" else [str(binary), case.kind, *case.args]


def run_complete(binary: Path, case: Case, timeout: float) -> subprocess.CompletedProcess[bytes]:
    env = {**os.environ, **case.env}
    return subprocess.run(
        command_for(binary, case),
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=env,
        cwd=case.env["PWD"],
        timeout=timeout,
        check=False,
    )


def measure_first_line(binary: Path, case: Case, timeout: float) -> float:
    env = {**os.environ, **case.env}
    start = time.perf_counter_ns()
    proc = subprocess.Popen(
        command_for(binary, case),
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=env,
        cwd=case.env["PWD"],
    )
    assert proc.stdout is not None
    try:
        line = proc.stdout.readline()
        elapsed_ms = (time.perf_counter_ns() - start) / 1_000_000.0
        if not line:
            stderr = proc.stderr.read(4096) if proc.stderr else b""
            raise RuntimeError(f"{case.name}: no stdout line; stderr={stderr!r}")
        return elapsed_ms
    finally:
        if proc.poll() is None:
            proc.terminate()
            try:
                proc.wait(timeout=timeout)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait(timeout=timeout)
        else:
            proc.wait(timeout=timeout)


def median(samples: list[float]) -> float:
    return float(statistics.median(samples))


def percentile(samples: list[float], pct: float) -> float:
    ordered = sorted(samples)
    if not ordered:
        return float("nan")
    index = min(len(ordered) - 1, max(0, round((pct / 100.0) * (len(ordered) - 1))))
    return float(ordered[index])


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline-binary", type=Path, default=Path("target/bench-baseline/historypwd-baseline"))
    parser.add_argument("--optimized-binary", type=Path, default=Path("target/release/historypwd"))
    parser.add_argument("--prepare-baseline", action="store_true", help="copy optimized binary to baseline path before running; refuses to overwrite unless --force-baseline is set")
    parser.add_argument("--force-baseline", action="store_true", help="allow --prepare-baseline to overwrite an existing baseline binary")
    parser.add_argument("--samples", type=int, default=30)
    parser.add_argument("--warmups", type=int, default=3)
    parser.add_argument("--timeout", type=float, default=10.0)
    parser.add_argument("--keep-fixtures", type=Path, help="write fixtures under this directory instead of a temporary directory")
    args = parser.parse_args()

    if args.samples < 1:
        parser.error("--samples must be >= 1")
    args.optimized_binary = args.optimized_binary.resolve()
    args.baseline_binary = args.baseline_binary.resolve()
    if not args.optimized_binary.exists():
        raise SystemExit(f"optimized binary not found: {args.optimized_binary}")
    if args.prepare_baseline:
        args.baseline_binary.parent.mkdir(parents=True, exist_ok=True)
        if args.baseline_binary.exists() and not args.force_baseline:
            print(f"baseline already exists, keeping it: {args.baseline_binary}", file=sys.stderr)
        else:
            shutil.copy2(args.optimized_binary, args.baseline_binary)
    if not args.baseline_binary.exists():
        raise SystemExit(
            f"baseline binary not found: {args.baseline_binary}; run with --prepare-baseline before optimizing"
        )

    if args.keep_fixtures:
        fixture_root = args.keep_fixtures
        if fixture_root.exists():
            shutil.rmtree(fixture_root)
        fixture_root.mkdir(parents=True)
        temp_cm = None
    else:
        temp_cm = tempfile.TemporaryDirectory(prefix="historypwd-first-line-")
        fixture_root = Path(temp_cm.name)

    try:
        fixture = make_fixtures(fixture_root)
        compatibility_ok = True
        compat_results = []
        for case in fixture.cases:
            base = run_complete(args.baseline_binary, case, args.timeout)
            opt = run_complete(args.optimized_binary, case, args.timeout)
            ok = (base.returncode, base.stdout, base.stderr) == (opt.returncode, opt.stdout, opt.stderr)
            compatibility_ok = compatibility_ok and ok
            compat_results.append(
                {
                    "scenario": case.name,
                    "ok": ok,
                    "baseline_returncode": base.returncode,
                    "optimized_returncode": opt.returncode,
                    "stdout_bytes": len(opt.stdout),
                    "stderr_bytes": len(opt.stderr),
                }
            )
            print(
                "compat "
                f"scenario={case.name} ok={str(ok).lower()} "
                f"stdout_bytes={len(opt.stdout)} stderr_bytes={len(opt.stderr)}"
            )
            if not ok:
                print(f"DIFF scenario={case.name}", file=sys.stderr)
                print(f"baseline stdout={base.stdout!r}", file=sys.stderr)
                print(f"optimized stdout={opt.stdout!r}", file=sys.stderr)
                print(f"baseline stderr={base.stderr!r}", file=sys.stderr)
                print(f"optimized stderr={opt.stderr!r}", file=sys.stderr)

        bench_results = []
        for case in [c for c in fixture.cases if c.benchmark]:
            for _ in range(args.warmups):
                measure_first_line(args.baseline_binary, case, args.timeout)
                measure_first_line(args.optimized_binary, case, args.timeout)
            baseline_samples: list[float] = []
            optimized_samples: list[float] = []
            for sample_index in range(args.samples):
                if sample_index % 2 == 0:
                    baseline_samples.append(measure_first_line(args.baseline_binary, case, args.timeout))
                    optimized_samples.append(measure_first_line(args.optimized_binary, case, args.timeout))
                else:
                    optimized_samples.append(measure_first_line(args.optimized_binary, case, args.timeout))
                    baseline_samples.append(measure_first_line(args.baseline_binary, case, args.timeout))
            baseline_ms = median(baseline_samples)
            optimized_ms = median(optimized_samples)
            improvement = ((baseline_ms - optimized_ms) / baseline_ms * 100.0) if baseline_ms else 0.0
            result = {
                "scenario": case.name,
                "baseline_ms": baseline_ms,
                "optimized_ms": optimized_ms,
                "improvement_pct": improvement,
                "baseline_p90_ms": percentile(baseline_samples, 90),
                "optimized_p90_ms": percentile(optimized_samples, 90),
                "samples": args.samples,
            }
            bench_results.append(result)
            print(
                "bench "
                f"scenario={case.name} "
                f"baseline_ms={baseline_ms:.3f} optimized_ms={optimized_ms:.3f} "
                f"improvement_pct={improvement:.2f} samples={args.samples} "
                f"baseline_p90_ms={result['baseline_p90_ms']:.3f} optimized_p90_ms={result['optimized_p90_ms']:.3f}"
            )

        summary = {
            "compatibility_ok": compatibility_ok,
            "samples": args.samples,
            "warmups": args.warmups,
            "baseline_binary": str(args.baseline_binary),
            "optimized_binary": str(args.optimized_binary),
            "fixture_root": str(fixture.root),
            "compatibility": compat_results,
            "benchmarks": bench_results,
        }
        print("summary_json " + json.dumps(summary, sort_keys=True))
        return 0 if compatibility_ok else 2
    finally:
        if temp_cm is not None:
            temp_cm.cleanup()


if __name__ == "__main__":
    raise SystemExit(main())
