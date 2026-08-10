"""a program that does nothing, so what is left is the cost of attaching

every other workload here pays this one's cost as well as its own, so the
difference between this row and another row of the same mode is roughly what
that workload's own work cost under that debugger

it has no `run` row: there is nothing here for a program to spend time on, and a
row measuring that would be measuring `time.perf_counter` twice
"""

import sys
import time

STARTED = time.perf_counter()

print(f"bpd-bench {(time.perf_counter() - STARTED) * 1_000_000:.0f}", flush=True)
sys.exit(0)
