"""a tight loop over many distinct lines — the worst case for `LINE` events

six lines of the loop body run three million times, so a debugger that reports
every line of an instrumented code object is asked about eighteen million
locations. `sys.monitoring.DISABLE` is the claim that it is asked about six

the two lines marked below are where the breakpoints go when this workload is
run with one. both live in the hottest function in the program, and between them
they separate the cost of *holding* a breakpoint from the cost of *hitting* one:
the first is reached fifty times out of three million iterations, the second is
reached never, because the branch above it cannot be taken. a row with a
breakpoint on the second one pays for instrumenting the code object and for no
stops at all

the exit code is the answer, so a run whose arithmetic came out wrong is not
counted as a fast one, and the printed line is the program's own clock over its
own work — the `run` rows, where the cost of starting a session is not counted
as the cost of running a line
"""

import sys
import time

ROUNDS = 3_000_000
EVERY = 60_000
ANSWER = 54999


def churn(rounds):
    total = 0
    for index in range(rounds):
        a = total + 1
        b = a * 2
        c = b - a
        if index % EVERY == 0:
            total = c + 1  # breakpoint reached fifty times
            if total < 0:
                total = EVERY  # breakpoint reached never
        total = (total + c + a) % 1000003
    return total


STARTED = time.perf_counter()
RESULT = churn(ROUNDS)

print(f"bpd-bench {(time.perf_counter() - STARTED) * 1_000_000:.0f}", flush=True)
sys.exit(0 if RESULT == ANSWER else 1)
