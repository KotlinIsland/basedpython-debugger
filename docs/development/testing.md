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

## coverage

```sh
cargo llvm-cov --workspace --all-features --summary-only
```

CI enforces a floor with `--fail-under-lines`. the floor exists to catch a
change that adds a body of untested code, not to be optimised against — a test
written to move the number rather than to establish a fact is worse than no
test, because it makes the number lie

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
worth catching, and a flaky perf gate teaches people to ignore perf failures.
instruction-count measurement is the way to make that gate meaningful, and it is
not wired up yet

## what is not covered yet

everything that needs a debuggee. there is no agent and no engine, so there is
nothing to stop, step, or inspect. the harness described here is the foundation
those tests will be written on — see `M1` in the roadmap
