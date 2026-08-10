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
   ├─ stage the agent build, from the cache after the first time
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

## where the agent is staged

the agent is a `cdylib`, and cargo names the artifact `libbpd_agent.dylib`,
`libbpd_agent.so` or `bpd_agent.dll`. an interpreter imports a module by file
name, so staging is that rename into a directory that goes on the debuggee's
`PYTHONPATH`

that directory is a **cache**, with one entry per distinct agent build:

```text
~/.cache/bpd/agents/<sha-256 of the artifact>/bpd_agent.so
```

`$XDG_CACHE_HOME` is used instead of `~/.cache` when it is set to an absolute
path, and windows uses `%LOCALAPPDATA%\bpd\agents\…\bpd_agent.pyd`. macOS gets
`~/.cache` rather than `~/Library/Caches`, because one rule is one thing to
check and the directory is `bpd`'s own either way

it used to be a fresh temporary directory per launch, and that was **119 ms of a
150 ms attach**: on macOS the first load of a shared object the system has never
seen is checked before it is mapped, and a copy written a moment ago is never a
file the system has seen. [what bpd costs](overhead.md) has the measurement, and
what it became

### the name is the content

a rebuilt agent has different bytes, so it has a different path, so no launch
can be handed a stale copy of an agent that has since been rebuilt. an mtime, a
version string or a build id each leave a case where the file changed and the
name did not, and running against code that is not the code in front of you is
the failure this project can least afford — the same class as `cargo test` not
rebuilding the `cdylib`, which has already cost this project a day

the entry is **checked rather than assumed**, too: its bytes are compared with
the artifact's before it is used, so an entry that a full disk truncated is
republished instead of imported. that read costs a fraction of a millisecond
against the 119 ms the cache is there to save

### publishing an entry

the file is written under a temporary name in the cache, flushed to disk, and
**renamed** into place. rename is atomic, so a second `bpd` launching at the
same moment sees either no file or the whole file — never a partial shared
object — and since the name is the content, whichever writer wins wrote the same
bytes as the one that lost

windows refuses to replace a file another process has loaded, which is exactly
what a debuggee running this agent looks like. the rename failing is therefore
not decisive: the entry is read, and a file that already holds the right bytes
is the request already satisfied

### the cache directory is a security boundary

what is in it is a shared object that gets loaded into the user's **own**
processes. a directory another user can write to is another user choosing what
runs inside the debuggee. so before anything is read from it or written to it:

| the directory | what happens |
| --- | --- |
| is not there | created, mode `0700` |
| is a link | refused |
| is not a directory | refused |
| belongs to another user | refused, naming both uids |
| is writable by group or other | refused, naming the mode |

write rather than read: the agent is not a secret, and a rule that refused
`0755` — what an ordinary umask produces — would be refusing directories nobody
else can put a file in

there is no fallback. a cache that cannot be trusted is a refusal naming the
path and the reason, not a quiet return to staging per launch, because falling
back would hide a broken cache behind a performance regression nobody notices

**on windows the ownership and mode checks are not made.** reading an ACL needs
a security descriptor walk that is not written here, and writing one against a
platform nobody on this project can inspect by hand would be a check nobody
could vouch for. what stands in for it is `%LOCALAPPDATA%`, which windows keeps
per-user, and the refusal of a junction pointing out of it. that is weaker than
the unix check, and this page says so rather than implying a check that does not
happen

### what removes an entry

nothing does. each is one copy of the agent — about a megabyte — and a
developer who rebuilds the agent leaves one behind every time. deleting the
directory is always safe: the next launch republishes what it needs and pays the
cold load once

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

## what the entry stop is

a stop of the whole program, and the only one that is. no user thread exists yet
— the program has run nothing — so the one thread that is held is the only thread
there is

every later stop holds one thread and leaves the rest running, including on a
gil-enabled build: the agent gives the GIL back for the duration of a stop rather
than freezing the process by accident. that is the model, and its costs are in
[threads](threads.md)

## the transport

loopback tcp, on a port the operating system chooses. a unix socket would be
better on unix and a named pipe is a second implementation of the same thing on
windows; this is one implementation that works everywhere

what makes loopback acceptable is the **session token**: 32 random bytes,
generated per launch, handed to the agent through its environment, compared in
constant time during the handshake. any local process can connect to the port;
none can join the session
