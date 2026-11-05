"""Compare separately built base and candidate index_remap examples."""
import argparse
import json
import os
import platform
import statistics
import subprocess

parser = argparse.ArgumentParser()
parser.add_argument('--base', default='/tmp/index-base')
parser.add_argument('--candidate', default='/tmp/index-candidate')
args = parser.parse_args()
rows = []
for files in [16, 1024, 10000]:
    for contexts in [1, 8]:
        iterations = max(20, 100000 // files)
        samples = {'base': [], 'candidate': []}
        for pair in range(10):
            order = ['base', 'candidate'] if pair % 2 == 0 else ['candidate', 'base']
            for build in order:
                command = [getattr(args, build), str(files), str(iterations), str(contexts)]
                samples[build].append(float(subprocess.check_output(command, text=True)))
        rows.append(dict(files=files, contexts=contexts, iterations=iterations,
                         samples_ms=samples,
                         median_ms={k: statistics.median(v) for k, v in samples.items()}))
print(json.dumps(dict(platform=platform.platform(), cpus=os.cpu_count(),
                      load=os.getloadavg(), rows=rows), indent=2))
