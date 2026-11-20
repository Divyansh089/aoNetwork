#!/usr/bin/env python3
"""Instruction counts of the bench workloads under Callgrind (NET-24).

count OUT
    Builds benches/instructions.rs, runs every workload under Callgrind with
    ASLR off, and writes {workload: instructions} to OUT as JSON. Writes
    nothing if this checkout has no such bench.
compare BASE HEAD LIMIT
    Prints each workload's change from BASE to HEAD and fails if any count
    grew by more than LIMIT, a fraction. A missing BASE compares nothing.
"""

import json
import os
import platform
import subprocess
import sys
import tempfile

BENCH = "instructions"
# Only instructions inside this function are counted: the workload itself,
# not building its inputs.
TOGGLE = f"{BENCH}::measure*"


def has_bench():
    metadata = subprocess.run(
        ["cargo", "metadata", "--no-deps", "--format-version=1"],
        check=True, capture_output=True, text=True,
    )
    targets = json.loads(metadata.stdout)["packages"][0]["targets"]
    return any(t["name"] == BENCH and "bench" in t["kind"] for t in targets)


def bench_binary():
    build = subprocess.run(
        ["cargo", "bench", "--bench", BENCH, "--no-run", "--message-format=json"],
        check=True, capture_output=True, text=True,
    )
    for line in build.stdout.splitlines():
        message = json.loads(line)
        if message.get("reason") == "compiler-artifact" and message["target"]["name"] == BENCH:
            if message.get("executable"):
                return message["executable"]
    sys.exit(f"cargo built no {BENCH} executable")


def total(callgrind_out):
    with open(callgrind_out) as lines:
        for line in lines:
            if line.startswith(("summary:", "totals:")):
                return int(line.split()[1])
    sys.exit(f"no totals in {callgrind_out}")


def count(out):
    if not has_bench():
        print(f"no {BENCH} bench in this checkout; nothing counted")
        return
    binary = bench_binary()
    names = subprocess.run(
        [binary, "--list"], check=True, capture_output=True, text=True
    ).stdout.split()
    counts = {}
    with tempfile.TemporaryDirectory() as tmp:
        for name in names:
            callgrind_out = os.path.join(tmp, f"{name}.out")
            subprocess.run(
                ["setarch", platform.machine(), "-R", "valgrind", "--tool=callgrind",
                 f"--toggle-collect={TOGGLE}", f"--callgrind-out-file={callgrind_out}",
                 binary, name],
                check=True, capture_output=True,
            )
            counts[name] = total(callgrind_out)
            if counts[name] == 0:
                sys.exit(f"{name}: nothing counted; is {TOGGLE} still the measured function?")
            print(f"{name}: {counts[name]}")
    with open(out, "w") as file:
        json.dump(counts, file, indent=2, sort_keys=True)


def compare(base_path, head_path, limit):
    base = {}
    if os.path.exists(base_path):
        with open(base_path) as file:
            base = json.load(file)
    with open(head_path) as file:
        head = json.load(file)
    worse = []
    for name in sorted(head):
        if name not in base:
            print(f"{name}: {head[name]} (new)")
            continue
        change = head[name] / base[name] - 1
        print(f"{name}: {base[name]} -> {head[name]} ({change:+.2%})")
        if change > limit:
            worse.append(name)
    if worse:
        sys.exit(f"more than {limit:.0%} more instructions than the base: " + ", ".join(worse))


if __name__ == "__main__":
    if sys.argv[1:2] == ["count"] and len(sys.argv) == 3:
        count(sys.argv[2])
    elif sys.argv[1:2] == ["compare"] and len(sys.argv) == 5:
        compare(sys.argv[2], sys.argv[3], float(sys.argv[4]))
    else:
        sys.exit(__doc__)
