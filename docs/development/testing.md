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
cargo test --workspace --all-features
```

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

## what is not covered yet

everything that needs a debuggee. there is no agent and no engine, so there is
nothing to stop, step, or inspect. the harness described here is the foundation
those tests will be written on — see `M1` in the roadmap
