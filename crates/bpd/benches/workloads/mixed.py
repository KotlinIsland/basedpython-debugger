"""the least synthetic one: parse a log, group it, summarise it, hash the result

a loop written to expose an event path is a loop nobody runs. this is ordinary
python — a compiled regex, dict and list building, a comprehension, `sorted`,
`statistics`, `json` and `hashlib` — so most of its time is inside C that a
`LINE` event never sees, and the python lines between those calls are few

that makes it the workload where a debugger's overhead should matter *least*,
which is the reason to measure it: a design that only looks good on a loop of
arithmetic is a design that only looks good on a benchmark
"""

import hashlib
import json
import re
import statistics
import sys
import time

ROUNDS = 120
BATCH = 2_000
ANSWER = "265ec62e574d801170a62fc59922dd1e68ec7e913d57f316ecf9920774cc95a3"

ENTRY = re.compile(
    r"(?P<host>\S+) - \[(?P<when>[^\]]+)\] "
    r'"(?P<verb>[A-Z]+) (?P<path>\S+)" (?P<status>\d{3}) (?P<size>\d+)'
)


def synthesise(count):
    return [
        f"10.0.{index % 255}.{index % 97} - [2026-08-10T{index % 24:02d}:00:00] "
        f'"GET /page/{index % 500}" {500 if index % 11 == 0 else 200} {index % 4096}'
        for index in range(count)
    ]


def parse(lines):
    parsed = []
    for line in lines:
        found = ENTRY.match(line)
        if found is None:
            continue
        fields = found.groupdict()
        fields["status"] = int(fields["status"])
        fields["size"] = int(fields["size"])
        parsed.append(fields)
    return parsed


def summarise(records):
    by_path = {}
    for record in records:
        by_path.setdefault(record["path"], []).append(record["size"])
    return {
        path: {"count": len(sizes), "mean": statistics.fmean(sizes), "max": max(sizes)}
        for path, sizes in sorted(by_path.items())
    }


def main(rounds):
    digest = hashlib.sha256()
    for _ in range(rounds):
        summary = summarise(parse(synthesise(BATCH)))
        digest.update(json.dumps(summary, sort_keys=True).encode())
    return digest.hexdigest()


STARTED = time.perf_counter()
RESULT = main(ROUNDS)

print(f"bpd-bench {(time.perf_counter() - STARTED) * 1_000_000:.0f}", flush=True)
sys.exit(0 if RESULT == ANSWER else 1)
