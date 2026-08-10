# testing

a debugger's tests have an unusual burden. the thing under test is another
process, running an interpreter whose behaviour changes between releases and
between build configurations, and the failure mode this project cares about
most — reporting something confidently and wrongly — is invisible to a test
that only checks the happy path

so the suite is built around three rules

## 1 — no silent skip

a test that quietly passes because no interpreter was installed reports success
while proving nothing. that is the same shape as the bugs this project exists to
prevent, in the tooling that is supposed to catch them

`bpd_test::discovered().require()` fails when there is no supported interpreter,
and the failure names what it did find and how to install one:

```text
no supported python interpreter was found, so this test would prove nothing

bpd requires cpython 3.13.0 or newer. what answered the probe:
    /usr/bin/python3 -> 3.9.13 (cpython)
install one:
    uv python install 3.13 3.14
or name them explicitly:
    BPD_TEST_PYTHONS=/path/to/python3.14 cargo test
```

a test that asserts on a **refusal** uses `.all()` instead, which includes the
interpreters `bpd` will not drive — those tests stay meaningful on a machine
with nothing but a python 3.9

## 2 — ground truth comes from somewhere else

a probe that agrees with itself proves nothing. every assertion about an
interpreter obtains its expected value by a different route than the code under
test:

| under test | ground truth |
| --- | --- |
| the version in the capability report | `sys.version`, the interpreter's own banner |
| `EXT_SUFFIX` in the report | `sysconfig.get_config_var` in a separate process |
| the free-threaded build flag | `sys._is_gil_enabled()` at runtime |
| `require_debuggable` | the version and implementation, compared independently |

`bpd_test::eval` exists for this. it runs a snippet in the interpreter and
returns its output, so a test can establish a fact rather than restate one

the free-threading check is one directional on purpose. a free-threaded build
re-enables the gil when an unprepared extension is imported, so
`sys._is_gil_enabled()` being true says nothing — but it being **false** proves
the build is free-threaded. the test asserts only the direction that is sound,
which is also the direction that would matter: claiming free-threading falsely

## 3 — the interpreter matrix is real interpreters

`bpd_test` discovers them by probing candidate command names, most specific
first, and deduplicating on `sys.executable` so a versioned name wins over
whichever `python3` points at the same binary. `BPD_TEST_PYTHONS` overrides the
list, delimited the way `PATH` is on the platform

CI gives each interpreter its own job rather than expecting one machine to hold
the whole matrix, so a version specific failure names the version in the job
title

## running the suite

```sh
cargo build -p bpd_agent
cargo test --workspace --all-features
```

**the build step is not optional.** the agent is a `cdylib` and nothing in the
workspace links it, so `cargo test` does not build it — it builds the agent's
lib target as a *test* binary instead, which produces no importable module

with no artifact at all the suite fails loudly, naming the build command. the
dangerous case is the other one: an artifact left by an earlier build is used
as-is, so an edit to `crates/bpd_agent/` can be "verified" against the previous
agent and appear to pass. every test touching the agent is then measuring
something that is not the code in front of you

against a specific interpreter:

```sh
BPD_TEST_PYTHONS=/path/to/python3.14 cargo test --workspace
```

## building the agent

the agent is a cpython extension module and it is **not** `abi3` — it reads
`sys.monitoring` and interpreter state whose layout changes between releases, so
one build is loadable by one `major.minor`

which interpreter it is compiled against comes from `PYO3_PYTHON`, resolved by
pyo3's own build script before anything in this workspace runs. that is why it
cannot be chosen from a manifest. `.cargo/config.toml` sets a default so the
workspace builds out of the box, and it is overridable:

```sh
PYO3_PYTHON=python3.13 cargo build -p bpd_agent
```

building against an interpreter older than the minimum fails in `build.rs`
rather than producing an artifact that could never work

**a successful import is not an ABI check.** on unix an extension module is a
shared object whose cpython symbols resolve at load time, so cpython 3.13
imports a 3.14 build without complaint and then runs it against a layout it was
not compiled for — which is worse than failing. `bpd_agent.verify_interpreter()`
is the check that actually decides, and
`crates/bpd_agent/tests/loads.rs` asserts that no interpreter other than the one
it was built for ever gets past it

the agent's own tests drive a real interpreter with the built artifact staged on
`PYTHONPATH`, because nothing in the workspace can link a `cdylib`.
`bpd_test::agent::staged()` does the staging — the rename cargo's artifact name
needs to become an importable module

## coverage

```sh
cargo llvm-cov --workspace --all-features --exclude bpd_agent --exclude bpd_test --summary-only
```

measured over the crates that ship. `bpd_test` is the harness, and covering a
harness measures nothing about the product; `bpd_agent` executes inside a python
process where llvm-cov cannot attribute it, and has its own tests instead

CI enforces a floor with `--fail-under-lines`. the floor exists to catch a
change that adds a body of untested code, not to be optimised against — a test
written to move the number rather than to establish a fact is worse than no
test, because it makes the number lie

## the launch baseline

running a program under a debugger has to be indistinguishable from running it
directly. `bpd_test::debuggee` runs a fixture the three ways cpython can be
entered and returns what the program observed about its own launch, so
`bpd launch` has something exact to be compared against

the three forms are not variations of one another:

| | `script.py` | `-m module` | `-c source` |
| --- | --- | --- | --- |
| `sys.argv[0]` | the path as given | **the resolved file path** | `-c` |
| `sys.path[0]` | the script's directory | the working directory | `""` |
| `__main__.__spec__` | absent | the module's spec | absent |
| `__main__.__package__` | absent | `""` | absent |
| `__main__.__file__` | the script | the module's file | absent |

these are recorded in `crates/bpd_test/tests/launch_forms.rs` against every
installed interpreter. two of them are the traps: `-m` rewrites `argv[0]` to the
resolved *file*, not the module name, and `-c` leaves `sys.path[0]` as the
empty string, which means "the working directory at import time" and is not the
same as the working directory spelled out

fixtures must never be run with `-I` or `-E`. isolated mode drops the script's
directory from `sys.path` entirely, which is one of the values under test

## benchmarks

```sh
cargo bench --workspace
```

benchmarks live in the tree, because a performance claim in a commit message is
not evidence. `crates/bpd_protocol/benches/frame.rs` measures the control plane
framing across the payload sizes the protocol actually carries — this is the
baseline the message encoding decision needs, since "json is fast enough" is a
claim about how much of a round trip is framing rather than serialisation

CI runs benchmarks with criterion's `--test` mode, which executes each one once
to catch a panic or a regression that stops it compiling. it does **not** gate
on wall-clock numbers from a shared runner: those vary by more than the effects
worth catching, and a flaky perf gate teaches people to ignore perf failures

## the performance gate that does run in CI

a number that is the same on every machine can be asserted on, and an
allocation count is such a number. `bpd_test::alloc` counts allocations per
thread, so a test can state exactly what a hot path is allowed to do:

```rust
let (present, allocations) = measure(|| read_frame_into(&mut wire, &mut buffer));
assert_eq!(allocations.count, 0);
```

this is what protects the documented claim that `read_frame_into` takes the
caller's buffer so a long lived reader reuses one allocation. that claim is
either enforced or it is a comment

the gate has been checked against a real regression: replacing the buffer reuse
with a fresh `vec![0; length]` per frame fails
`crates/bpd_protocol/tests/allocation.rs` with the count it saw — while **every
functional test still passes**, which is precisely the class of change that
would otherwise ship unnoticed

a test binary using this must install the allocator, and
`Allocations::assert_measured` exists because a binary that forgot to reports
zero for everything — the same value the assertions are looking for:

```rust
#[global_allocator]
static ALLOCATOR: bpd_test::alloc::Counting = bpd_test::alloc::Counting;
```

## proving a stop is real

a debugger that reported a stop a moment too late would look identical from the
outside, so no test here takes the agent's word for one. the fixture programs
write a marker on a line **after** the breakpoint, and every stop asserts that
the marker does not say what that line would have made it say

`crates/bpd_engine/tests/stop_and_resume.rs` does it for the entry stop and
`crates/bpd_engine/tests/breakpoints.rs` for every breakpoint stop. the same
discipline covers the line tables: the expected executable lines and offsets
come from `co_lines()` in a separate interpreter process, so nothing about a
line table is written down in rust and compared against itself

**the tests are checked against the regressions they exist to catch.** removing
the `co_consts` recursion fails nine of them; removing the `restart_events()`
call after a breakpoint change fails exactly one, which is the one written for
it; keeping cpython's line 0 in the line table fails the line table test. for
what a breakpoint carries: treating a condition that raised as false fails two,
counting hits the condition rejected fails one, answering a name that is not a
local as false instead of handing it to the interpreter fails the differential
corpus, reusing a hit counter for a breakpoint that changed fails one and never
reusing one fails its opposite, and a transport counter that does not count
fails the logpoint round trip test. a test that cannot be made to fail is not
evidence

**one guard here cannot be made to fail, and that is written down rather than
left looking like coverage.** the agent suppresses its own breakpoints while a
condition is being evaluated, and cpython already refuses to re-enter a tool's
callback on a thread that is inside one — so deleting the suppression changes
nothing observable. the interpreter's behaviour is therefore pinned directly, in
a bare interpreter with no agent involved, by
`the_interpreter_does_not_report_an_event_raised_from_inside_a_callback`. if
that ever changes, the tests around it start being able to fail, and the
suppression becomes the thing holding the line

## what is not covered yet

the two adapters — neither of them exists

stepping, pausing and the exception breakpoints are covered by
`crates/bpd_engine/tests/stepping.rs` and
`crates/bpd_engine/tests/exceptions.rs`. every call a step might or might not
enter writes a marker file, so a step over asserts the callee's marker **is**
there and a step in asserts it is **not** — the same claim checked from both
sides, with nothing taken from the agent

the breaks they were checked against: taking the `restart_events()` off a step
fails the disabled-line test, which then sees the program run to its end rather
than land anywhere; following the code object rather than the frame fails the
recursion test and the generator test; letting a line be disabled while another
thread is stepping fails the held-open cross-thread test; not taking a step off
when a breakpoint stops the thread fails the breakpoint-wins test; reporting a
raise every time cpython raises the event fails the "one exception is one stop"
test; and reporting an unwind before it reaches the outermost frame fails both
uncaught tests

three of their assertions are about **cpython** rather than about `bpd`, and are
made in a bare interpreter: that one `raise` produces a raise event in every
frame the exception passes through, that an exception escaping a thread's target
is caught by `threading` itself, and that clearing a code object's local events
undoes its disables — the cheaper instrument a step deliberately does not use

**one guard here cannot be made to fail on a gil-enabled build**, and is written
down rather than left looking like coverage. `PY_START` is not disabled while a
step in is in flight, because a code object the interpreter has been told never
to report again is one the step would never be offered — and a step in that was
never offered the frame it entered behaves exactly like a step over. the window
is a handful of bytecodes on the stepping thread, and no other thread runs in it
while the GIL exists. on a free-threaded build it is a real window, which is why
the guard is there

the thread model does, and it is covered by `crates/bpd_engine/tests/threads.rs`.
that file is the first place multi-threaded fixtures are the point rather than
something avoided, and the way it stays deterministic is that the threads
coordinate through **files** rather than through timing: a worker waits for a
file the test writes, and the test waits for a file the worker writes. a slow
machine makes it slower, not flakier

nothing in it takes the agent's word for anything. that a thread is running is
proved by a file it wrote **while another thread was held**; that a thread is
held is proved by a file that did not appear. see [threads](threads.md)

the breaks those tests were checked against: not releasing the GIL while stopped
fails the progress test and hangs the concurrent-stop one, resuming every thread
where one was named fails the per-thread resume test, reporting nothing for a
stop inside an import fails the import test, never reporting a thread as still
fails both lock tests, counting a thread parked in a C call as held fails the
stop-the-world test, reporting the wrong mode fails it too, ignoring a resume for
a thread that is not held fails the refusal test, and reporting an empty held
list as the program ends fails the finishing test

frames, scopes and values are covered by `crates/bpd_engine/tests/state.rs`, and
two of its assertions are about **cpython** rather than about `bpd`, so they are
made in a bare interpreter: that `f_locals` accepts and reads back a write the
compiled code can never see — which is the whole reason a write is refused
unless the name is already in the scope — and that a module frame's locals and
globals are the same object. see [the stopped state](state.md)

the breaks those tests were checked against: not stopping the stack walk at the
bootstrap frame fails three, reading a free variable from the globals fails the
scope test, dropping the scope check before a write fails the refusal test,
dropping cycle detection fails the cycle test, reading a list's length through
`__len__` instead of its storage fails the storage test, calling `__repr__`
without being asked fails the repr test, calling an unreadable name unbound fails
the class body test, and reading a scope at the depth asked for rather than the
depth the budget fits fails two

conditions and hit counters are tested on one thread. a counter is shared by
every thread that reaches its breakpoint, and there is still no test of two
threads counting the same one — the harness for a deterministic threaded fixture
now exists, in `threads.rs`, so the reason is that nobody has written it rather
than that it cannot be written
