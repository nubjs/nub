#!/usr/bin/env python3
"""Linux/systemd acceptance: activation, explicit overrides, Workers, and fork."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile

parser = argparse.ArgumentParser()
parser.add_argument("--nub", required=True, type=Path)
parser.add_argument("--node-dir", type=Path)
args = parser.parse_args()
nub = str(args.nub.resolve())
fixture = Path(__file__).resolve().parents[1] / "fixtures/gc-startup/main.cjs"


def checked(command):
    return subprocess.check_output(command, text=True)


with tempfile.TemporaryDirectory(prefix="gc-acceptance-") as temporary:
    root = Path(temporary)
    downloads = args.node_dir or root / "nodes"
    downloads.mkdir(parents=True, exist_ok=True)
    cases = 0
    for version in ["22.23.2", "24.20.0", "26.8.1"]:
        name = f"node-v{version}-linux-x64"
        archive = downloads / f"{name}.tar.xz"
        url = f"https://nodejs.org/dist/v{version}/"
        if not archive.exists():
            subprocess.run(["curl", "-fsS", url + archive.name, "-o", str(archive)], check=True)
        sums = checked(["curl", "-fsS", url + "SHASUMS256.txt"])
        digest = next(line.split()[0] for line in sums.splitlines() if line.split()[-1] == archive.name)
        assert hashlib.sha256(archive.read_bytes()).hexdigest() == digest
        subprocess.run(["tar", "xf", str(archive), "-C", str(downloads)], check=True)
        node = str((downloads / name / "bin/node").resolve())
        project = root / version
        project.mkdir()
        (project / "home").mkdir()
        (project / "main.cjs").write_bytes(fixture.read_bytes())
        (project / "pressure.cjs").write_bytes((Path(__file__).parent / "pressure.cjs").read_bytes())
        checksums = set()
        config = {"nodeExecutable": node}
        (project / "nub.jsonc").write_text(json.dumps(config))
        (project / "package.json").write_text(json.dumps({
            "private": True, "scripts": {"probe": "node main.cjs"},
        }))
        early = """
if (require('node:worker_threads').isMainThread) {
  const worker = new (require('node:worker_threads').Worker)(
    `require('node:worker_threads').parentPort.postMessage(require('node:v8').getHeapStatistics().heap_size_limit/2**20)`,
    {eval:true,resourceLimits:{maxYoungGenerationSizeMb:8,maxOldGenerationSizeMb:128}});
  worker.once('message', heap => require('node:assert/strict').equal(heap,140));
}
"""
        (project / "early.cjs").write_text(early)
        bins = project / "node_modules/.bin"
        bins.mkdir(parents=True)
        binary = bins / "gc-probe"
        binary.write_text("#!/usr/bin/env node\n" + fixture.read_text())
        binary.chmod(0o755)

        def run(label, command, memory=512, extra_env=()):
            global cases
            env = ["PATH=" + str(Path(node).parent) + ":/usr/bin:/bin",
                   "HOME=" + str(project / "home"), *extra_env]
            invocation = [
                "sudo", "-n", "systemd-run", "--quiet", "--wait", "--pipe", "--collect",
                "--uid=" + str(os.getuid()), "--working-directory=" + str(project),
                "-p", f"MemoryMax={memory}M", "-p", "MemorySwapMax=0",
                "-p", "RuntimeMaxSec=90", "-p", "LimitCORE=0",
                "/usr/bin/env", "-i", *env,
            ]
            result = subprocess.run(invocation + command, text=True, capture_output=True, timeout=110)
            assert result.returncode == 0, (version, label, result.stdout, result.stderr)
            # Package-script runners may print the script name before its JSON.
            data = json.loads(result.stdout.strip().splitlines()[-1])
            assert data["node"] == "v" + version, data
            if label == "nub" and memory in tuned and data["mainHeap"] != tuned[memory]:
                diagnostic = subprocess.run(invocation + [node, "-e", r"""
const fs = require('fs'), path = require('path');
console.log(fs.readFileSync('/proc/self/cgroup','utf8'));
console.log(fs.readFileSync('/proc/self/mountinfo','utf8').split('\n').filter(x=>x.includes(' - cgroup')).join('\n'));
const group = fs.readFileSync('/proc/self/cgroup','utf8').split('\n').find(x=>x.startsWith('0::')).slice(3);
for(let dir='/sys/fs/cgroup'+group;dir.startsWith('/sys/fs/cgroup');dir=path.dirname(dir)) {
  for(const key of ['memory.max','memory.high']) {
    try { console.log(dir+'/'+key,fs.readFileSync(dir+'/'+key,'utf8').trim()); }
    catch(error) { console.log(dir+'/'+key,error.code); }
  }
}
"""], text=True, capture_output=True, timeout=110)
                raise AssertionError((data, diagnostic.stdout, diagnostic.stderr))
            if "checksum" in data:
                checksums.add(data["checksum"])
                assert len(checksums) == 1, data
            if label == "nub":
                assert data["fork"]["mainHeap"] == defaults[memory], data
            cases += 1
            print(json.dumps({"version": version, "case": label, "memory": memory,
                              "mainHeap": data["mainHeap"], "forkHeap": data.get("fork", {}).get("mainHeap"),
                              "peakRssMiB": data.get("peakRssMiB"), "memoryEvents": data.get("memoryEvents")}), flush=True)
            return data["mainHeap"]

        defaults = {}
        tuned = {}
        ceiling = {"22.23.2": 1024, "24.20.0": 512, "26.8.1": 0}[version]
        for memory in [256, 384, 500, 511, 512, 513, 640, 768, 1024, 1025, 1536, 2048, 2049, 4096]:
            defaults[memory] = run("node", [node, "main.cjs"], memory)
            expected = defaults[memory]
            if 512 <= memory <= ceiling:
                # Query the stock Node flag directly, without the Worker fixture:
                # the explicit global flag intentionally overrides Worker limits.
                expected = run("semi16-oracle", [node, "--max-semi-space-size=16", "-e",
                    "console.log(JSON.stringify({node:process.version,mainHeap:require('v8').getHeapStatistics().heap_size_limit/2**20}))"], memory)
                assert expected > defaults[memory], (version, memory, expected, defaults[memory])
                tuned[memory] = expected
            assert run("nub", [nub, "--no-check", "main.cjs"], memory) == expected
        startup_heap = tuned.get(512, defaults[512])
        for label, command, env, expected in [
            ("application-args", ["main.cjs", "--port=3000"], (), startup_heap),
            ("node-bin", ["exec", "gc-probe"], (), startup_heap),
            ("package-script", ["run", "probe"], (), defaults[512]),
            ("compat-argv", ["--node", "main.cjs"], (), defaults[512]),
            ("compat-env", ["main.cjs"], ("NODE_COMPAT=1",), defaults[512]),
            ("user-preload", ["main.cjs"], ("NODE_OPTIONS=--require ./early.cjs",), defaults[512]),
            ("user-heap-env", ["main.cjs"], ("NODE_OPTIONS=--max-semi-space-size=4",), 268),
            ("user-heap-argv", ["--max-semi-space-size=4", "main.cjs"], (), 268),
        ]:
            assert run(label, [nub, "--no-check", *command], extra_env=env) == expected
        (project / "nub.jsonc").write_text(json.dumps({**config, "v8Flags": ["--max-semi-space-size=4"]}))
        assert run("user-heap-config", [nub, "--no-check", "main.cjs"]) == 268
        (project / "nub.jsonc").write_text(json.dumps(config))
        (project / ".env").write_text("NODE_OPTIONS=--max-semi-space-size=4\n")
        # Runtime-control variables from .env are deliberately ignored by Nub.
        assert run("ignored-heap-dotenv", [nub, "--no-check", "main.cjs"]) == startup_heap
        (project / ".env").unlink()
        for label, settings in [
            ("user-conditions", {"conditions": ["development"]}),
            ("user-prefix", {"prefix": ["/usr/bin/env"]}),
            ("user-config-preload", {"preload": ["./early.cjs"]}),
        ]:
            (project / "nub.jsonc").write_text(json.dumps({**config, **settings}))
            assert run(label, [nub, "--no-check", "main.cjs"]) == defaults[512]
        (project / "nub.jsonc").write_text(json.dumps(config))
        for memory in sorted({512, *tuned}):
            expected = tuned.get(memory, defaults[memory])
            assert run("pressure-node", [node, "pressure.cjs", str(memory)], memory) == defaults[memory]
            assert run("pressure-nub", [nub, "--no-check", "pressure.cjs", str(memory)], memory) == expected
    print(f"GC_ACCEPTANCE_OK {cases} cases")
