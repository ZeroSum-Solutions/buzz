#!/usr/bin/env python3
"""Agent-minutes per wave, per workflow and per stage, from Claude Code workflow transcripts.

This is the measurement behind the "Measured baseline" and "Second baseline" sections of
docs/plans/2026-09-05-zs-throughput-plan.md. Definitions (unchanged since the first baseline):

  one agent        = one agent-*.jsonl transcript inside a workflow directory
  agent-minutes    = last timestamp minus first timestamp of that transcript, rounded per agent
  stage            = the role line of the agent's prompt (first record of the transcript), matched
                     in the priority order of STAGES below (specific roles before the generic BUILDER)
  wave             = the set of workflow ids listed for it in the manifest

The transcripts are machine-local and are never committed; only this script and the manifest
(workflow ids, labels, expected totals) live in the repo.

Usage:
  agent-minutes.py --manifest agent-minutes-manifest.json --wave waves-2-3 [--workflows-dir DIR]
  agent-minutes.py --manifest agent-minutes-manifest.json --check      # regression: every wave with an expected total
"""
import argparse
import datetime as dt
import glob
import json
import os
import re
import sys
from collections import OrderedDict, defaultdict

STAGES = [
    ("delta audit", r"DELTA AUDIT RUNNER"),
    ("verifier", r"adversarial verifier"),
    ("port check", r"PORT CHECK"),
    ("pr opener", r"PR OPENER"),
    ("critic", r"BLIND CRITIC"),
    ("tester", r"TEST RUNNER"),
    ("fix", r"FIX agent"),
    ("audit", r"adversarial senior code reviewer|Review the memo"),
    ("builder", r"BUILDER"),
]


def parse_ts(s):
    return dt.datetime.fromisoformat(s.replace("Z", "+00:00"))


def read_agent(path):
    """Return (first_ts, last_ts, stage) for one transcript, or None if it has no timestamps."""
    first = last = None
    stage = None
    with open(path, encoding="utf-8") as fh:
        for i, line in enumerate(fh):
            try:
                rec = json.loads(line)
            except json.JSONDecodeError:
                continue
            ts = rec.get("timestamp")
            if ts:
                t = parse_ts(ts)
                first = t if first is None else min(first, t)
                last = t if last is None else max(last, t)
            if stage is None and i < 3:
                blob = json.dumps(rec)
                for name, pat in STAGES:
                    if re.search(pat, blob):
                        stage = name
                        break
    if first is None:
        return None
    return first, last, stage or "unclassified"


def measure(workflows_dir, wf_ids):
    rows = []
    for wf in wf_ids:
        matches = glob.glob(os.path.join(workflows_dir, wf + "*"))
        if not matches:
            print(f"warning: no directory for {wf} under {workflows_dir}", file=sys.stderr)
            continue
        for path in sorted(glob.glob(os.path.join(matches[0], "agent-*.jsonl"))):
            r = read_agent(path)
            if r is None:
                continue
            first, last, stage = r
            rows.append({
                "wf": wf,
                "agent": os.path.basename(path)[6:-6],
                "first": first,
                "last": last,
                "minutes": round((last - first).total_seconds() / 60),
                "stage": stage,
            })
    return rows


def report(name, wave, rows):
    by_wf = OrderedDict()
    by_stage = defaultdict(lambda: [0, 0])
    for r in rows:
        d = by_wf.setdefault(r["wf"], {"n": 0, "min": 0, "first": r["first"], "last": r["last"]})
        d["n"] += 1
        d["min"] += r["minutes"]
        d["first"] = min(d["first"], r["first"])
        d["last"] = max(d["last"], r["last"])
        by_stage[r["stage"]][0] += 1
        by_stage[r["stage"]][1] += r["minutes"]
    total = sum(r["minutes"] for r in rows)
    print(f"== {name}: {len(rows)} agents, {total} agent-minutes ==")
    print("workflow          label                                 agents  agent-min  first            last")
    for wf, d in by_wf.items():
        label = wave["workflows"].get(wf, "")[:36]
        print(f"{wf:17s} {label:37s} {d['n']:5d} {d['min']:10d}  {d['first']:%m-%dT%H:%MZ}  {d['last']:%m-%dT%H:%MZ}")
    print("stage         runs  agent-min")
    for stage, (n, m) in sorted(by_stage.items(), key=lambda kv: -kv[1][1]):
        print(f"{stage:13s} {n:4d} {m:10d}")
    return total


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--manifest", required=True)
    ap.add_argument("--wave", help="wave name from the manifest")
    ap.add_argument("--workflows-dir", help="directory holding wf_* transcript directories (default: manifest workflows_dir)")
    ap.add_argument("--check", action="store_true", help="regression: compare every wave that has expected_agent_minutes")
    args = ap.parse_args()

    with open(args.manifest, encoding="utf-8") as fh:
        manifest = json.load(fh)
    wdir = os.path.expanduser(args.workflows_dir or manifest["workflows_dir"])
    if not os.path.isdir(wdir):
        sys.exit(f"workflows dir not found: {wdir}")

    if args.check:
        failed = False
        for name, wave in manifest["waves"].items():
            exp = wave.get("expected_agent_minutes")
            if exp is None:
                continue
            total = report(name, wave, measure(wdir, list(wave["workflows"])))
            tol = wave.get("tolerance", 0)
            ok = abs(total - exp) <= tol
            failed |= not ok
            print(f"CHECK {name}: measured {total}, expected {exp} (tolerance {tol}) -> {'OK' if ok else 'FAIL'}\n")
        sys.exit(1 if failed else 0)

    if not args.wave:
        sys.exit("pass --wave NAME or --check")
    wave = manifest["waves"][args.wave]
    report(args.wave, wave, measure(wdir, list(wave["workflows"])))


if __name__ == "__main__":
    main()
