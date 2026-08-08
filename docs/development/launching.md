# launching a debuggee

`bpd launch` runs a program with the agent already attached, holds it before its
first statement, and lets it go

```sh
bpd launch --python python3.14 script.py --and its arguments
```

there is nothing yet that could be told about the stop or asked what to do next,
so the stop happens and the program is resumed immediately. what it establishes
is the part everything else needs: the agent attaches, the program is genuinely
held before it has run, and letting it go produces a run indistinguishable from
a bare one

## the shape of a launch

```text
  bpd                                      the debuggee
   │
   ├─ probe the interpreter, refuse if it cannot be driven
   ├─ stage the agent build into a directory
   ├─ bind loopback, generate a session token
   ├─ spawn ────────────────────────────▶  python -c "import bpd_agent;
   │                                                   bpd_agent.main()"
   │                                        │
   │                                        ├─ verify the interpreter matches
   │                                        ├─ read the endpoint and token
   │                                        ├─ erase them from the environment
   │  ◀──────── connect, handshake ─────────┤
   │                                        ├─ claim the monitoring tool id
   │                                        ├─ arm PY_START
   │                                        ├─ repair argv and sys.path[0]
   │                                        ├─ install a fresh __main__
   │  ◀──────── stopped: entry ─────────────┤  ← the program has run nothing
   ├─ resume ──────────────────────────────▶│
   │                                        └─ run the program
   └─ exit with the program's own code
```

the agent connects **back** to the engine. that removes any race over who binds
first, and means the debuggee never listens for anything

## why a `-c` bootstrap and not a python file

the interpreter has to be entered somehow, and every option leaves a trace. a
`sitecustomize` would be inherited by every subprocess; a bootstrap file would
put launch semantics in a second place where they can be subtly wrong, and leave
its own name in `sys.modules`

so the entry point is the shortest thing that can work —
`import bpd_agent; bpd_agent.main()` — and everything after it is rust

## what `-c` breaks, and what puts it back

entering through `-c` is not the launch form the user asked for, and the
differences are visible to the program:

| | what `-c` leaves | what the program expects |
| --- | --- | --- |
| `sys.argv[0]` | `-c` | the script path **as typed** |
| `sys.path[0]` | `""` | the script's directory |
| `__main__` | the bootstrap's module | the program's own |
| `__file__` | absent | the script, absolutised |

two of these are easy to get subtly wrong. `sys.argv[0]` is the path **as
typed** — cpython does not absolutise it, though it does absolutise `__file__`,
so the two disagree on purpose. and `sys.path[0]` is the script's *directory*,
not the working directory; the two coincide often enough that only a test which
separates them proves which one the launcher used

none of this is asserted from memory.
`crates/bpd_test/tests/launch_forms.rs` records what a bare interpreter
produces, and `crates/bpd/tests/launch_parity.rs` runs the same program twice —
once bare, once under `bpd` — and compares. no expected value is written down,
because the expected value is whatever cpython does

## failures belong to the program

a program that cannot be compiled, or a script that cannot be opened, never
reaches its first statement. `bpd` adds nothing to what the interpreter would
have said:

- a `SyntaxError` prints with its line, its source text and its caret, and the
    traceback holds the program's frames and none of `bpd`'s
- an unreadable script is reported with `os.strerror` and exits **2**, which is
    what cpython uses — not the 1 an uncaught exception produces
- an uncaught exception is printed by cpython's own printer, so a program that
    installed its own `sys.excepthook` still gets it

the engine reports these as `ExitedBeforeStopping` and propagates the exit code
without a word of its own. a line of `bpd`'s on top would be a line that is not
there without the debugger

## the one fingerprint that remains

`bpd_agent` stays in the debuggee's `sys.modules`. it cannot be removed —
unimporting it would unload the code that is running. everything else is
erased: the endpoint, the token and the target path are taken out of the
environment before any user code runs, so a program cannot tell it is being
debugged by reading `os.environ`

this is recorded as an assertion rather than a footnote, in
`the_program_is_the_only_main_module`

## what the entry stop is, and is not

the agent holds the GIL while it is stopped. at an **entry** stop that is a
complete stop of the whole program, because no user thread exists yet — the
program has run nothing

it is not sufficient for a breakpoint. by then other threads are running, and
holding the GIL only stops the ones that want it. real stop coordination is its
own piece of work, and it lands with breakpoints

## the transport

loopback tcp, on a port the operating system chooses. a unix socket would be
better on unix and a named pipe is a second implementation of the same thing on
windows; this is one implementation that works everywhere

what makes loopback acceptable is the **session token**: 32 random bytes,
generated per launch, handed to the agent through its environment, compared in
constant time during the handshake. any local process can connect to the port;
none can join the session
