"""ten million python calls — the worst case for the global `PY_START`

`PY_START` is the only way PEP 669 offers to discover a code object: there is no
"code object created" event, so a session with a breakpoint in it arms
`PY_START` for the whole program and registers each code object the first time
it is entered. the design says that costs one native callback per code object,
*once*, because the callback returns `DISABLE`

this workload is what tells the two apart. four calls per iteration across three
functions is ten million entries into three code objects, so a `PY_START` that
disabled itself is invisible here and one that did not is catastrophic
"""

import sys
import time

ROUNDS = 2_500_000
ANSWER = 875027


def leaf(value):
    return value + 1


def middle(value):
    return leaf(value) + leaf(value)


def outer(value):
    return middle(value) - leaf(value)


def run(rounds):
    total = 0
    for index in range(rounds):
        total = (total + outer(index)) % 1000003
    return total


STARTED = time.perf_counter()
RESULT = run(ROUNDS)

print(f"bpd-bench {(time.perf_counter() - STARTED) * 1_000_000:.0f}", flush=True)
sys.exit(0 if RESULT == ANSWER else 1)
