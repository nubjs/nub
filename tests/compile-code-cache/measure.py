"""Measure warm short-command startup on Unix; keep setup outside timed runs."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import random
import resource
import statistics
import subprocess
import tempfile
import time

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--arm", action="append", required=True, help="name=/absolute/path/to/executable")
parser.add_argument("--out", required=True)
parser.add_argument("--rounds", type=int, default=21)
parser.add_argument("--argument", default="--version")
args = parser.parse_args()
arms = dict(arm.split("=", 1) for arm in args.arm)
assert args.rounds > 0 and len(arms) == len(args.arm)
identities = {}
for name, file in arms.items():
    binary = Path(file).resolve()
    arms[name] = str(binary)
    identities[name] = dict(path=str(binary), bytes=binary.stat().st_size,
        sha256=hashlib.sha256(binary.read_bytes()).hexdigest())
env = {k: v for k, v in os.environ.items() if not k.startswith(("NODE_", "NUB_", "__NUB_"))}
rows = []
outputs = {}
rng = random.Random(5709)
report = dict(platform=platform.platform(), identities=identities, argument=args.argument,
    warmups=2, rounds=args.rounds, load_before=os.getloadavg(), rows=rows)
try:
    with tempfile.TemporaryDirectory(prefix="compile-cache-bench-") as cache:
        for round in range(-2, args.rounds):
            names = list(arms)
            rng.shuffle(names)
            for name in names:
                child_env = dict(env, NODE_COMPILE_CACHE=str(Path(cache) / name))
                before = resource.getrusage(resource.RUSAGE_CHILDREN)
                start = time.perf_counter()
                child = subprocess.run([arms[name], args.argument], env=child_env,
                    capture_output=True, timeout=30)
                wall_ms = (time.perf_counter() - start) * 1000
                after = resource.getrusage(resource.RUSAGE_CHILDREN)
                row = dict(arm=name, round=round, wall_ms=wall_ms,
                    user_ms=(after.ru_utime-before.ru_utime)*1000,
                    sys_ms=(after.ru_stime-before.ru_stime)*1000,
                    rc=child.returncode, stdout=child.stdout.decode(errors="replace"),
                    stderr=child.stderr.decode(errors="replace"))
                if round >= 0:
                    rows.append(row)
                assert child.returncode == 0, row
                assert outputs.setdefault(name, child.stdout) == child.stdout, row
finally:
    report["load_after"] = os.getloadavg()
    Path(args.out).write_text(json.dumps(report, indent=2) + "\n")
for name in arms:
    values = sorted(row["wall_ms"] for row in rows if row["arm"] == name)
    print(name, dict(min=values[0], p25=values[len(values)//4], median=statistics.median(values), max=values[-1]))
