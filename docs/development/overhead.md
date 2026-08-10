# what bpd costs

`README.md` used to say that a line with no breakpoint on it costs nothing,
because the location is `DISABLE`d the first time it is seen. that was an
argument from architecture with no number behind it, which by this project's own
rules is a claim that should not have been made

`crates/bpd/benches/overhead.rs` is the number. this page is what it said, and
not all of it agreed with the pages it was written to check

## what is measured

five programs, chosen to be the **worst** cases for this design rather than the
best. they live in `crates/bpd/benches/workloads/`:

| workload | what it is for |
| --- | --- |
| `startup` | a program that does nothing, so what is left is the cost of attaching |
| `lines` | six lines of a loop body run three million times — eighteen million `LINE` locations |
| `calls` | ten million python calls across three functions — the worst case for the global `PY_START` |
| `imports` | fifty-odd stdlib packages — thousands of code objects, each seen once |
| `mixed` | a log parsed with a regex, grouped, summarised, hashed — ordinary python, most of its time inside C |

each is run three ways — bare, under `bpd`, and under debugpy — and `lines` is
run twice more under each debugger, once with a breakpoint that is hit fifty
times and once with a breakpoint on a line the program never reaches. both
debuggers are driven through **DAP**, by the same client, so what is compared
includes each one's own front end rather than only its event path

three groups come out of it:

- **session** — wall clock for the whole thing: spawn, attach, the program, the
    disconnect. what somebody waits for
- **run** — the program's own clock over its own work, printed by the program
    and read back off whichever channel the debugger gives its stdout. the fixed
    cost of starting a session lands outside this, so it is the number the claim
    about a *line* is a claim about
- **attach** — where `bpd`'s fixed cost goes

nothing takes a debugger's word for anything. every workload's exit code is the
answer it computed, and the benchmark asserts it is zero; the breakpoint rows
assert the breakpoint bound, bound to the line asked for, and was hit exactly as
often as the program reaches it. a debugger whose breakpoint silently failed to
bind would otherwise post the best number on the page

## the machine

| | |
| --- | --- |
| cpu | apple M4 Max — 12 performance cores, 4 efficiency cores |
| memory | 128 GiB |
| os | macOS 26.5.2 (25F84), arm64 |
| interpreter | cpython 3.14.7 |
| debugpy | 1.8.21 |
| rust | 1.97.0, `--release` |

taken on an otherwise idle machine. every figure is the **median of ten runs**,
with the smallest and largest of the ten in brackets — ten whole processes, not
ten iterations inside one, because what is being measured takes hundreds of
milliseconds and the variation that matters in a process-level measurement is
between processes

**these are one machine's numbers.** nobody has run this on linux or windows,
and the largest single component of the fixed cost below is a macOS behaviour

to reproduce:

```sh
cargo build --release -p bpd_agent
BPD_BENCH_DEBUGPY=/path/to/venv/bin/python cargo bench --bench overhead
```

## the whole session, in milliseconds

| workload | bare | bpd | debugpy |
| --- | --- | --- | --- |
| `startup` | 11 (10–11) | 163 (155–258) | 1030 (1009–1046) |
| `lines` | 178 (177–179) | 354 (350–359) | 1043 (1021–1069) |
| `calls` | 228 (225–233) | 403 (385–414) | 1251 (1220–1265) |
| `imports` | 62 (62–63) | 204 (196–205) | 1015 (1002–1069) |
| `mixed` | 264 (262–269) | 429 (426–441) | 1244 (1209–1250) |

and `lines` again, with one breakpoint in the function the loop is in:

| | bpd | debugpy |
| --- | --- | --- |
| hit fifty times | 361 (337–399) | 11319 (11061–14178) |
| never hit | 343 (336–347) | 11315 (11185–11591) |

## the program's own run, in milliseconds

| workload | bare | bpd | debugpy |
| --- | --- | --- | --- |
| `lines` | 164 (162–165) | 165 (163–168) | 167 (166–168) |
| `calls` | 216 (213–255) | 214 (213–216) | 215 (213–219) |
| `imports` | 43 (43–44) | 40 (39–42) | 18 (18–21) |
| `mixed` | 250 (248–257) | 252 (247–254) | 251 (249–258) |

| `lines`, with one breakpoint | bpd | debugpy |
| --- | --- | --- |
| hit fifty times | 180 (179–185) | 10384 (10203–10529) |
| never hit | 165 (164–167) | 10285 (10060–10465) |

## attaching, in milliseconds

| | |
| --- | --- |
| the interpreter alone, `python -c pass` | 9.5 (9.4–9.6) |
| the agent imported, staged once | 10.2 (10.0–10.7) |
| the agent imported, staged fresh | 129 (127–147) |

## what the numbers say

### the claim holds for the event path, and the never-hit row is the proof

`lines` runs eighteen million line locations. with a breakpoint **held in the
function that runs them**, the program's own run is 165 ms against 164 ms bare —
0.7%, inside the run-to-run spread of the bare row itself. six of those eighteen
million locations were ever reported to the debugger; the rest returned
`DISABLE` on their first pass and were never offered again

that is the strongest form of the claim, and it is the version worth stating: it
is not that an *unattached* line is free, it is that a line in the same code
object as a live breakpoint is free. `calls` says the same thing about the
global `PY_START` — ten million entries into three code objects, and the run is
if anything slightly faster than bare

### but "costs nothing" was never the whole cost, and the docs never said what the rest was

attaching costs 140–180 ms before the program's first statement. that is not on
the event path and it is not what `DISABLE` is about, but it is what somebody
waits for, and no page said it

the `attach` rows say where it goes. importing the agent costs **0.7 ms**. the
first load of a *freshly written* copy of it costs **119 ms more than the same
file loaded again**. `bpd` stages a new copy of the agent into a new temporary
directory on every launch, so it pays that on every launch

it is a fix waiting to happen — staging to a stable, content-addressed path
would turn a 150 ms attach into something near 30 ms — and it is written down in
`ROADMAP.md` rather than fixed here, because measuring a thing and changing it
in the same breath leaves nothing to compare against. **it is also macOS**,
where a shared object the system has not seen is validated on first load; what
linux and windows charge for the same thing is unmeasured

### with a breakpoint in a hot function, the architecture is the whole difference

31× end to end: 361 ms against 11 319 ms, same program, same interpreter, same
breakpoint, both hit fifty times

the never-hit row says where that comes from. with a breakpoint on a line the
program **never reaches**, debugpy's run is 10 285 ms against 164 ms bare —
**63×**. hitting the breakpoint fifty times adds about 100 ms on top of that,
which is 2 ms a stop; `bpd`'s fifty stops add 15 ms, which is 0.3 ms a stop

so almost none of it is stopping. it is every *other* line of the code object
the breakpoint lives in, reported to the debugger and answered, three million
times over. that is the thing `sys.monitoring.DISABLE` exists for, and it is the
one measurement on this page that is really about architecture rather than about
implementation

### debugpy is not a `settrace` debugger any more, and this project's docs said it was

debugpy 1.8.21 vendors pydevd with `_pydevd_sys_monitoring`, compiled with
cython, and uses it on cpython 3.12 and newer. `PYDEVD_USE_SYS_MONITORING` is
`IS_PY312_OR_GREATER and hasattr(sys, "monitoring")`

so on every interpreter `bpd` supports, debugpy is *also* a PEP 669 debugger
with a native callback. and it shows: with no breakpoints set its run rows are
within 2% of bare, exactly as `bpd`'s are. what separates the two is what
happens once a breakpoint is set, and the fixed cost of a session — 1.0 s
against 0.15 s

`docs/index.md` used to say debugpy meant "a python callback on every line of
every frame that is being traced". that is true of the interpreters debugpy
still supports and `bpd` does not, and it is not true of any interpreter this
project runs on. it has been rewritten to say what was measured instead

### the `imports` run row does not measure what it was built to

43 ms bare, 40 ms under `bpd`, **18 ms under debugpy**. a program is *faster* at
importing the stdlib under a debugger, because the debugger has already imported
part of it — pydevd pulls in a large slice of what this workload asks for, and
those imports are already in `sys.modules` before the program starts

the row is kept, and this paragraph is why: deleting a measurement that came out
awkwardly is how a benchmark turns into an advertisement. what it is evidence of
is module cache warmth, not the event path. the `session` row for `imports` is
unaffected and is the honest end-to-end number

## what is not measured

- **linux and windows.** the benchmark runs there; nobody has recorded what it
    says. the fixed cost above is macOS's
- **free-threaded builds.** the agent releases the GIL for the whole of a stop
    and the registry behind one is native, so a free-threaded build has a
    different profile and no figure here
- **more than one thread.** every workload is single threaded
- **more than one breakpoint**, breakpoints spread across code objects,
    conditions, hit counts, and logpoints. what a condition costs depends on
    what the expression is, and the fast path for a comparison against a local
    has a differential test rather than a number
- **stepping.** a step arms `PY_UNWIND` for the whole program and calls
    `restart_events()`, which re-enables every disabled location in the process.
    that is the most expensive thing `bpd` does, and it has no benchmark
- **attach**, which is not built

## why CI does not gate on any of this

it does not, and it must not start to. wall clock from a shared runner varies by
more than the effects worth catching, and a flaky performance gate teaches
people to ignore performance failures

what CI runs is criterion's `--test` mode, which executes every row once. that
catches a benchmark that stops compiling, panics, or fails one of the assertions
above — including a breakpoint that stopped binding, which is a correctness
regression this benchmark would notice

the gate that *is* deterministic is the allocation count in
`crates/bpd_protocol/tests/allocation.rs`, because an allocation count is the
same number on every machine. see [testing](testing.md)
