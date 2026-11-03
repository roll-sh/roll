#!/usr/bin/env python3
"""Attribute perf samples (perf script -F tid,ip,sym, demangled) under a function to its
direct callees: `stacks.py FILE FUNC [N]` prints the share of all samples each child of
FUNC (the frame just below its outermost occurrence) takes, inclusive."""
import sys, collections

path, func = sys.argv[1], sys.argv[2]
n = int(sys.argv[3]) if len(sys.argv) > 3 else 25
samples, stack = [], []
for line in open(path, errors="replace"):
    line = line.rstrip("\n")
    if not line.strip():
        if stack:
            samples.append(stack)
        stack = []
        continue
    if line.startswith("\t") or line.startswith(" " * 4):
        parts = line.strip().split(" ", 1)
        stack.append(parts[1] if len(parts) > 1 else "?")
if stack:
    samples.append(stack)
total = len(samples)
inside = 0
kids = collections.Counter()
for s in samples:
    idx = [i for i, f in enumerate(s) if func in f]
    if not idx:
        continue
    inside += 1
    i = max(idx)  # outermost occurrence (stacks are leaf first)
    kids[s[i - 1] if i > 0 else "(self)"] += 1
print(f"{func}: {100 * inside / total:.1f}% of {total} samples")
for k, c in kids.most_common(n):
    print(f"  {100 * c / total:5.1f}%  {k[:150]}")
