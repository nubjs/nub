#!/usr/bin/env python3
"""Score each tracer's output against the known-answer fixture, by unique token.

The fixture exits non-zero unless every op actually happened, so a missing
token means the TRACER missed it. t01_created is the positive control: a
mechanism that misses it is misconfigured and its other rows mean nothing.
"""
import sys

# token, what it proves
CASES = [
    ("t01_created",     "create + write (positive control)"),
    ("t04_renamedest",  "rename DESTINATION"),
    ("t05_linkdest",    "hard-link destination"),
    ("t06_linkatdest",  "linkat(dirfd) destination"),
    ("t07_symlinkdest", "symlink destination"),
    ("t08_madedir",     "mkdir"),
    ("t11_mmapdest",    "MAP_SHARED|PROT_WRITE target"),
    ("t12_childwrite",  "forked child's write (process tree)"),
    ("t13_execchild",   "exec'd grandchild's write (process tree)"),
]

def load(p):
    try:
        return open(p, errors="replace").read()
    except OSError:
        return ""

def main():
    arms = [(n, load(p)) for n, p in (a.split("=", 1) for a in sys.argv[1:])]
    w = max(len(t) for t, _ in CASES) + 2
    print("token".ljust(w) + "".join(n.ljust(20) for n, _ in arms) + "proves")
    print("-" * (w + 20 * len(arms) + 40))
    tally = {n: 0 for n, _ in arms}
    for tok, why in CASES:
        row = tok.ljust(w)
        for n, body in arms:
            hit = tok in body
            tally[n] += hit
            row += ("SEEN" if hit else "MISSED").ljust(20)
        print(row + why)
    print()
    for n, _ in arms:
        print(f"{n}: {tally[n]}/{len(CASES)}")
    # A mechanism that misses the positive control is misconfigured.
    for n, body in arms:
        if "t01_created" not in body:
            print(f"!! {n} MISSED THE POSITIVE CONTROL — its other rows are meaningless")

main()
