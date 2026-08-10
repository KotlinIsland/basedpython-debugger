# python support

## the policy

| | version | why |
| --- | --- | --- |
| minimum | cpython 3.13 | PEP 669 is the only event backbone |
| attach | cpython 3.14 | PEP 768 `sys.remote_exec` |
| implementation | cpython only | PEP 669 and PEP 768 are cpython interfaces |

there is no compatibility layer, no capability fallback ladder, and no branch
anywhere in the tree that keeps an older interpreter partly working. an
interpreter either has what a feature needs or `bpd` reports that it does not,
by name, and stops

`bpd doctor` answers this for any interpreter:

```sh
bpd doctor python3.14
```

## why 3.13 and not 3.12

PEP 669 landed in **3.12**, so 3.12 is the version a compatibility-minded
project would pick

3.13 is the version a correctness-minded project picks, for two reasons

the first is **PEP 667**. before 3.13, `frame.f_locals` handed back a snapshot
dict, and writing to it did not change the frame. every debugger on 3.12 and
earlier either silently discarded an assignment to a local or reached for a
private C API to make it stick. 3.13 made `f_locals` a write-through proxy, so
"set this variable and continue" means what it says. a debugger that cannot
honestly write a local is a debugger with a whole category of lies in it

the second is `sys.monitoring` itself. 3.12's implementation had behavioural
gaps around generators, exception unwinding and the interaction between
`DISABLE` and local events, several of which were fixed as bug fixes rather
than as documented behaviour changes. supporting 3.12 means carrying a second
set of stepping rules and a second set of expectations in the test suite, for
one release, forever

that is exactly the kind of compromise this project exists to avoid. the cost
of the decision is paid by users on 3.12, once, in a clear error message

## why cpython only

PEP 669 and PEP 768 are cpython interfaces. pypy, graalpy and the rest either do
not implement them or implement something that looks similar and behaves
differently under jit compilation and deoptimisation

a debugger that reports *approximately* the right frame on an alternative
implementation is worse than one that refuses. `bpd doctor` names the
implementation it found and stops

## free-threaded builds

a `Py_GIL_DISABLED` build is a first-class target, not a variant to be handled
later — **from 3.14**. pyo3 does not support the free-threaded build of any
cpython below that, so `3.13t` cannot be built against at all and the build
fails saying so. free-threading is therefore a 3.14 feature here, and the
minimum for it is not the minimum for the rest

a free-threaded build is a **different abi**, not a variant of the same one, and
`sys.version_info` reports the same `3.14` for both. so the stamp the agent
refuses a mismatched interpreter with carries the build too — `3.14t` — because
a version alone names two interpreters and would wave one of them through

the practical consequence is on the agent: nothing in it may be correct only
because the GIL serialised it. the registry of held threads, the breakpoint table
and the event counters are all explicitly synchronised. this is not extra work
for free-threading — the GIL never protected against a callback running on
another thread between two operations anyway. free-threading only makes the
existing bug reproducible

the same reasoning is why a stop **releases** the GIL rather than holding it. a
debugger whose threading behaviour depended on which build you had would be a
capability ladder, and this is the page that says there is not one. see
[threads](threads.md)

## what "no compromises" rules out, concretely

things that a python debugger normally does and `bpd` will not:

- a `sys.settrace` path, for any reason, on any version
- injecting into a process by ptrace, `gdb`, a signal handler, or writing to
    another process's memory outside the documented PEP 768 protocol
- monkeypatching stdlib modules to observe them
- guessing a frame's state from a traceback when the frame itself is gone
- reporting a value read from a different scope than the one asked for

## adopting a new cpython release

each release is treated as a possible behaviour change to the event model, not
as a compatibility exercise. what has to be checked, every time:

- the `sys.monitoring` event set. 3.14 split `BRANCH` into `BRANCH_LEFT` and
    `BRANCH_RIGHT`, which is the kind of change that silently drops a feature if
    nobody looks
- `DISABLE` semantics, and what `restart_events()` re-enables
- the code object layout that breakpoint binding walks, and what `co_lines()`
    yields. 3.12 inlined list, dict and set comprehensions into their enclosing
    function, which moved every breakpoint inside one to a different code
    object; a module's leading `RESUME` is reported as line 0, which is not a
    source line. the bytecode offsets a line covers differ between every release
    measured so far, which is why no test writes one down — the expected values
    come from `co_lines()` on the interpreter under test
- **the names the interpreter puts in `__main__`**, for each of the three
    launch forms. this has changed in each of the last three releases: 3.13
    gives `__main__` an `__annotations__` and 3.14 does not, and 3.15 removed
    `__cached__` from module namespaces entirely — from a script's `__main__`,
    from runpy's, and from every imported module. `bpd` builds the program's
    `__main__` and so has to match. it does it by **copying** what the
    interpreter already put in its own, and by asking the running interpreter
    whether a module it loaded from source carries `__cached__` — so what has to
    be checked here is whether a new release moved a name that neither of those
    two rules covers. `crates/bpd/tests/launch_parity.rs` fails when it has
- the PEP 768 debug offsets, which are version specific by design
- whether anything in the release makes a previously-required workaround
    unnecessary. removing one is as much a part of the upgrade as adding
    support
