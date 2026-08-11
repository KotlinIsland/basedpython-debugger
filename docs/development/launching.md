# launching a debuggee

`bpd launch` runs a program with the agent already attached, holds it before its
first statement, and lets it go

```sh
bpd launch --python python3.14 script.py --and its arguments
bpd launch --python python3.14 -m package.module --and its arguments
bpd launch --python python3.14 -c 'print("and its arguments")' --and them
```

there is nothing yet that could be told about the stop or asked what to do next,
so the stop happens and the program is resumed immediately. what it establishes
is the part everything else needs: the agent attaches, the program is genuinely
held before it has run, and letting it go produces a run indistinguishable from
a bare one

## the three forms

an interpreter can be entered three ways, and **none of them is a special case
of another**. what they differ in is visible to the program:

|                        | `script.py`            | `-m module`           | `-c source` |
| ---------------------- | ---------------------- | --------------------- | ----------- |
| `sys.argv[0]`          | the path as typed      | **the resolved file** | `-c`        |
| `sys.path[0]`          | the script's directory | the working directory | `""`        |
| `__main__.__spec__`    | absent                 | the module's spec     | absent      |
| `__main__.__package__` | absent                 | `""`, or the package  | absent      |
| `__main__.__file__`    | the script             | the module's file     | absent      |
| `__main__.__cached__`  | `None`                 | the `.pyc`            | absent      |

`__cached__` is in that table with a caveat: **cpython 3.15 removed it** from
module namespaces, from a script's `__main__`, from runpy's and from every
imported module. so the row is 3.13 and 3.14. the launcher does not carry a
version table for it — it asks the running interpreter whether a module it
loaded through `SourceFileLoader` carries the name, which is the same removal
seen from the same process. 3.13 has an `__annotations__` in `__main__` that
3.14 does not, and that one needs no rule at all: the program's `__main__`
starts as a **copy** of the one the interpreter built

two of those are traps. `-m` rewrites `sys.argv[0]` to the **file** the module
resolved to, not to the module name — so a program that reports its own
invocation shows a path nobody typed. and `-c` leaves `sys.path[0]` as the empty
string, which means "the working directory, resolved at import time": spelling
the working directory out instead looks identical until the program calls
`os.chdir`, and then it imports a different module, or none

`bpd launch` takes the interpreter's own argument vector for all three, so the
same words that follow `python` follow `bpd launch --python python`. `-m` and
`-c` each take the whole of the rest of the line, which is what the interpreter
does — `python -m pkg -c x` runs `pkg` with `-c x` as its arguments, and so does
this. that is also what makes the three forms exclusive without a conflict rule:
there is no arrangement of arguments in which two of them are given. giving
**none** of them is refused while parsing

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
 │                                        ├─ take its own directory back off
 │                                        │  PYTHONPATH and sys.path
 │                                        ├─ claim the monitoring tool id
 │                                        ├─ arm PY_START
 │                                        ├─ repair what the form needs
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

| the directory                 | what happens              |
| ----------------------------- | ------------------------- |
| is not there                  | created, mode `0700`      |
| is a link                     | refused                   |
| is not a directory            | refused                   |
| belongs to another user       | refused, naming both uids |
| is writable by group or other | refused, naming the mode  |

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

entering through `-c` is only the launch form the user asked for one time in
three, and the differences are visible to the program:

|               | what `-c` leaves       | a script                | `-m`                  |
| ------------- | ---------------------- | ----------------------- | --------------------- |
| `sys.argv[0]` | `-c`                   | the path **as typed**   | the resolved file     |
| `sys.path[0]` | `""`                   | the script's directory  | the working directory |
| `__main__`    | the bootstrap's module | the program's own       | the program's own     |
| `__file__`    | absent                 | the script, absolutised | the module's file     |

the command form is the one that needs nothing put back, and saying so is the
point: `-c` is entered through `-c`, so `sys.argv[0]` and `sys.path[0]` are
already what they should be. writing them anyway is how the empty string turns
into a working directory that stops tracking `os.chdir`

`sys.argv[0]` for a script is the path **as typed** — cpython does not
absolutise it, though it does absolutise `__file__`, so the two disagree on
purpose. and `sys.path[0]` is the script's *directory*, not the working
directory; the two coincide often enough that only a test which separates them
proves which one the launcher used

**`PYTHONSAFEPATH` and `-P` turn the prepending off entirely.** there is then no
`sys.path[0]` of the interpreter's to replace — slot zero is the first real
entry, which on a stock build is the stdlib zip — so the repair does nothing at
all. a launcher that wrote its entry anyway would hand the program a search path
a bare run never has, and take a stdlib entry out on the way

none of this is asserted from memory.
`crates/bpd_test/tests/launch_forms.rs` records what a bare interpreter
produces, and `crates/bpd/tests/launch_parity.rs` runs the same program twice —
once bare, once under `bpd` — and compares, for **all three forms**, by handing
the same argument vector to `python` and to `bpd launch`. no expected value is
written down, because the expected value is whatever cpython does

### `-m` is runpy's, whole

`bpd` does not resolve a module. it calls `runpy._run_module_as_main`, which is
private and is also exactly what cpython's own `pymain_run_module` calls — and
the reason it is the *whole* of what `bpd` does is measured rather than stylistic.
a bare `-m` traceback holds runpy's own frames:

```text
Traceback (most recent call last):
  File "<frozen runpy>", line 203, in _run_module_as_main
  File "<frozen runpy>", line 88, in _run_code
  File "/tmp/boom.py", line 1, in <module>
```

so resolving the module here and running the code object directly would produce
a traceback with **fewer** frames than a bare run, and resolving it once to learn
the file and then calling `_run_module_as_main` anyway would run a failing
package's `__init__` twice and report the failure from the wrong depth. that also
settles the package case, the missing-module message and the `argv[0]` rewrite
without any of them being a rule `bpd` wrote down

it does cost one thing, and it is what the entry stop had to be redesigned
around: nothing outside runpy knows which **file** the module resolved to until
runpy is already running it

### how the entry stop knows the program

the stop fires on the `PY_START` of the program's own body, so that a breakpoint
set during it has the whole main module — its functions, classes and
comprehensions — already registered to bind to. which code object that is, is
answered two ways because the forms genuinely differ in who compiles the program:

- a script and a `-c` command are compiled by `bpd`, so the code object **is**
    the program and it is recognised by identity
- `-m` is compiled inside runpy, so what identifies it is the file `__main__`
    names — a module namespace is complete before its body runs, so `__file__`
    is already there when the first `PY_START` from that file arrives

### the `<string>` a traceback would have shown

cpython keeps the source of a `-c` command in `linecache` so a traceback can
print the line it came from, keyed on the code object's filename, qualname and
first line. the bootstrap **is** a `-c` command, so `("<string>", "<module>", 1)`
holds `import bpd_agent; bpd_agent.main()` before the agent has run at all — and
`compile` names its code `<string>` by default

that is a wrong line, not a missing one: a traceback through anything the program
compiled would print `bpd`'s bootstrap as the program's source, with a caret
under it. the entry is removed for every form, and the command form then
registers its own source through the same function `pythonrun.c` uses

## failures belong to the program

a program that cannot be compiled, or a script that cannot be opened, never
reaches its first statement. `bpd` adds nothing to what the interpreter would
have said:

- a `SyntaxError` prints with its line, its source text and its caret, and the
    traceback holds the program's frames and none of `bpd`'s
- an unreadable script is reported with `os.strerror` and exits **2**, which is
    what cpython uses — not the 1 an uncaught exception produces
- a module that is not there is reported by runpy, in the wording and with the
    exit code runpy gives it: `<executable>: No module named nope`, exit **1**.
    a package that raises while it is being imported reports from inside runpy's
    resolution, one frame shallower than a failure in the module itself
- an uncaught exception is printed by cpython's own printer, so a program that
    installed its own `sys.excepthook` still gets it

the engine reports these as `ExitedBeforeStopping` and propagates the exit code
without a word of its own. a line of `bpd`'s on top would be a line that is not
there without the debugger

## the one fingerprint that remains

`bpd_agent` stays in the debuggee's `sys.modules`, and so do the modules
importing it pulled in. it cannot be removed — unimporting it would unload the
code that is running

everything else is erased before any user code runs:

- the endpoint, the token, the target and the form leave `os.environ`
- **`PYTHONPATH` is put back to what it was**, and the agent's staged directory
    comes off `sys.path`. that directory is how the bootstrap can import the
    agent at all, and both halves of it were visible: the variable in
    `os.environ`, and the directory on the search path — where under
    `PYTHONSAFEPATH` it lands in **slot zero**, ahead of the stdlib. a directory
    searched before everything else is the debugger deciding what the program
    imports. an inherited `PYTHONPATH` is prepended to rather than replaced, so
    a program does not lose search path it was given
- the `linecache` entry for the bootstrap's own source, above

this is recorded as assertions rather than footnotes, in
`the_program_is_the_only_main_module`,
`a_program_that_reads_its_own_environment_finds_no_debugger_in_it` and
`a_program_that_reads_its_own_import_path_finds_no_debugger_on_it`

what is **not** erased is `sys.modules`: importing the agent imports
`threading`, `re`, `sysconfig` and the rest of what it needs, and a program that
lists its own modules sees them. that is a difference `bpd` has not closed, and
it is written here rather than implied away

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
