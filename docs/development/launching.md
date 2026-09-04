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

## `--debug-children`

```sh
bpd launch --python python3.14 --debug-children manage.py runserver
```

the program's children become debuggees of their own instead of being reported
and left alone — the same setting DAP reaches as `debugChildren` and MCP as
`debug_children`, and [child processes](subprocesses.md#a-child-that-is-debugged)
is the design. it is **off** unless asked for, and the flag goes before the
program, like `--python`: everything from the first positional on is the
program's own

what this command does with a child is what it does with the program: says
where it is held, and lets it go. it has no ui to hold one in, and a child that
arrives held with nothing able to resume it is a hung program — which is why the
flag needed the sink before it needed the flag. it also waits for **every**
session before it returns, because leaving would close the connection to a child
that is still running and the agent answers a vanished debugger by ending the
process. [child processes](subprocesses.md#what-bpd-launch-debug-children-does-with-one)
has both, and the one difference this makes to when the command returns

the setting is asked for while the program is still held at its entry stop, so
it is in place before the program has run the line that could make a child. a
refusal — there is no `fork` on this platform — is a refusal before the program
ran, which is the rule an unsupported interpreter is refused by

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
 ├─ load a basedpython source map if the program runs out of a build
 │  directory, and refuse if it no longer describes what is on disk
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

the two things above the spawn are both refusals, and both are above it on
purpose: a program that ran and then could not be debugged is a program that
ran. an interpreter below the minimum is one of them, and
`crates/bpd/tests/launch_refusal.rs` is what says nothing was started. the other
is a basedpython build that no longer matches its source map — see
[source mapping](source-mapping.md), which is also where the rule that bpd
**finds** the map rather than being told where it is is written down. nothing
about it reaches the debuggee: the map is read and hashed out of process, and
the agent never learns one exists

## the interpreter that is started is the one that was named

the probe asks an interpreter about itself, and two of the answers are paths:
the one it was **named** by on the command line, and `sys.executable`, which is
what it says it is. bpd starts the first

they are the same almost everywhere, and on a macos framework build they are
two different files. measured on a runner:

```text
named   /…/Versions/3.13/Resources/Python.app/Contents/MacOS/Python
probed  sys.executable = /…/Versions/3.13/bin/python3.13
```

starting the second is starting **a file nobody named**, and everything the
debuggee then says about itself is that file's: its own `sys.executable`, the
interpreter its children are started with, and the name cpython prints in front
of its errors. `the_program_is_run_by_the_interpreter_bpd_was_given` compares
the debuggee's `sys.executable` against a bare run's, which is the difference
itself rather than a symptom of it

`sys.executable` is still what the agent tag and the child-process verdicts are
read from — it is what the interpreter says about itself, and that is a
different question from what to run

### and the name it puts in front of its own errors

a script the interpreter cannot open is refused by the agent rather than by
cpython, because under bpd the program is reached through the `-c` bootstrap —
so the agent writes that message, and it has to write the one cpython would:

```text
<name>: can't open file '<path>': [Errno 2] No such file or directory
```

`<name>` is **`sys.orig_argv[0]`**, the name the interpreter was invoked by, and
not `sys.executable`. cpython normalises the second and leaves the first alone,
which is visible without a framework build at all:

```text
$ /…/bin/../bin/python3.14 /absent.py
/…/bin/../bin/python3.14: can't open file '/absent.py': [Errno 2] …
$ /…/bin/../bin/python3.14 -c 'import sys; print(sys.executable)'
/…/bin/python3.14
```

reading `sys.executable` there is a debuggee that names itself differently than
it would have, and every macos job in ci failed on it —
`a_script_that_cannot_be_opened_is_refused_in_the_interpreters_own_words`
compares the two runs word for word, which is where it showed up

## which agent is staged

the agent is a cpython extension and is **not** `abi3`: it reads
`sys.monitoring` and interpreter internals whose layout changes between
releases. one build is loadable by one `(release, build configuration)` pair,
and a free-threaded interpreter is a different abi rather than a variant of the
same one — different struct layouts, different reference counting, and the same
`sys.version_info`. so `bpd` carries one agent per **interpreter tag**, spelled
the way cpython spells its own extension suffix: `3.13`, `3.14`, `3.14t`

they live beside the binary, one directory per tag:

```text
<prefix>/bin/bpd
<prefix>/agents/3.13/libbpd_agent.so
<prefix>/agents/3.14/libbpd_agent.so
<prefix>/agents/3.14t/libbpd_agent.so
```

both the directory holding `bpd` and the one above it are looked in, which is
why the same rule serves an installed `<prefix>/bin/bpd` and a test binary in
`target/debug/deps`

a directory per tag rather than a tag in the file name, for two reasons. the
artifact keeps the name **cargo** gave it, so whatever assembles the layout
copies and renames nothing — there is no step in which a name could be invented
that disagrees with the bytes beside it. and the tags a `bpd` carries are then
read off the filesystem rather than recovered by parsing file names, which is
what lets a refusal say what is really there.
`bpd_engine::agent::published_at` is where the layout is defined, and it is
what CI and the tests assemble one through

### how the choice is made

by the tag the interpreter reported **about itself** when it was probed — its
`sys.version_info` and its `Py_GIL_DISABLED` — never by what a path claims.
`EXT_SUFFIX` would work too and is what an earlier note proposed, but it also
carries a platform that a file on this machine cannot disagree about; the tag is
the vocabulary `bpd_agent.verify_interpreter()` is written in, so what selection
asks for is exactly what verification compares

**`verify_interpreter` has not gone anywhere.** selection picking the right file
and the agent checking it was compiled for the interpreter that imported it are
two different guarantees, and the second is what catches a wrong first — a
mismatched agent is refused at import, before anything is instrumented

### the development build

a checkout has no layout. `cargo build -p bpd_agent` leaves one artifact next to
the binary, and that is used for any interpreter no tagged agent was carried
for:

```sh
PYO3_PYTHON=python3.14 cargo build -p bpd_agent
cargo run --bin bpd -- launch --python python3.14 script.py
```

it is the weaker claim of the two, because nothing about it names an
interpreter — so a tagged agent wins wherever there is one, and the agent's own
check at import is what settles whether the untagged one fits. which is the
whole of what a checkout has ever relied on

### when there is no agent for an interpreter

the refusal names three things, because two of them are not enough: the
interpreter and the tag it needs, the tags that **are** carried, and what to do

```text
error: bpd carries no agent for python 3.13.5 (`python3.13`), which needs the
build tagged `3.13`. it carries `3.14` in `/opt/bpd/agents/3.14`, `3.15` in
`/opt/bpd/agents/3.15`. the agent is a cpython extension and is not abi3 — it
reads interpreter state whose layout changes between releases — so one build
loads into one release and one build configuration and no other. debug with an
interpreter this bpd carries, or build the agent for this one:
    PYO3_PYTHON=python3.13 cargo build -p bpd_agent
```

a `bpd` carrying nothing at all says that instead, and names both directories it
looked in — "no agent for python 3.13" would send someone hunting for a
packaging problem that is really an empty build tree

`crates/bpd/tests/agents.rs` is where all of this is asserted, and it is
asserted through a real `bpd` binary in a directory of its own with a layout
beside it rather than over a path string. only one tag can hold a real agent —
`cargo test` builds the agent against one interpreter — so the others hold bytes
that are not one, which makes the negative half exact: the entry a launch stages
into is the sha-256 of what it staged, so an interpreter that reached another
tag's directory names another entry

## where the agent is staged

the agent is a `cdylib`, and cargo names the artifact `libbpd_agent.dylib`,
`libbpd_agent.so` or `bpd_agent.dll`. an interpreter imports a module by file
name, so staging is that rename into a directory that goes on the debuggee's
`PYTHONPATH`

that directory is a **cache**, with one entry per distinct agent build:

```text
~/.cache/bpd/agents/<sha-256 of the artifact>/bpd_agent.so
```

carrying several agents changes nothing here. the entry is keyed on the bytes,
so several agents are simply several entries, and an entry still holds one file
named for the platform's import suffix and nothing else — which is the rule
[`bpd cache`](caches.md) reads a directory by

`$XDG_CACHE_HOME` is used instead of `~/.cache` when it is set to an absolute
path, and windows uses `%LOCALAPPDATA%\bpd\agents\…\bpd_agent.pyd`. macOS gets
`~/.cache` rather than `~/Library/Caches`, because one rule is one thing to
check and the directory is `bpd`'s own either way

**no launch ever removes an entry**, which is what makes reuse safe and also
what makes the directory grow without limit — 89 entries and 448 MiB of them on
the machine this was written on. reclaiming it is a thing a person asks for,
with [`bpd cache`](caches.md), and never something a launch does on its own

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

**a person, and nothing else.** [`bpd cache clear`](caches.md) is the only
thing that takes an entry out; no launch, no timer and no threshold does it, and
the reason is in that page

each entry is one copy of the agent — 5.6 MB of it, not the megabyte this said
before anyone measured — and a developer who rebuilds the agent leaves one
behind every time, which is how the cache on the machine this was written on
reached 89 entries and 448 MiB. deleting the whole directory by hand is always
safe too: the next launch republishes what it needs and pays the cold load once

## why a `-c` bootstrap and not a python file

the interpreter has to be entered somehow, and every option leaves a trace. a
`sitecustomize` would be inherited by every subprocess; a bootstrap file would
put launch semantics in a second place where they can be subtly wrong, and leave
its own name in `sys.modules`

so the entry point is the shortest thing that can work —
`import bpd_agent; bpd_agent.main()` — and everything after it is rust

### the child's side of the same choice

bpd does use a `sitecustomize`, in exactly one place: to enter a child that was
**`exec`'d**, when `debugChildren` was asked for. that is not a reversal of the
paragraph above, because it is a decision about a different process

what makes it wrong for the parent is what makes it the only thing that works for
a child. an `exec`'d child is a fresh interpreter with none of this process's
memory in it, so the only channels into a `python …` command line bpd did not
write are the environment and the files the interpreter reads at startup — and
"inherited by every subprocess" is the property that reaches one at all

the parent is launched by bpd, so it needs no such channel and gets none: the
directory holding that file goes on the parent's `PYTHONPATH` **after** `site`
has already run, which is why the parent never imports it and why
`the_only_modules_a_debuggee_gains_are_the_ones_written_down` did not change when
this landed. what a debugged child gains is enumerated separately, in
[child processes](subprocesses.md#what-a-debugged-child-gains)

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

## the debuggee's own standard streams

the rule is the same one the rest of this page follows, applied to a channel
that is easy to forget is program-observable: **`bpd` gives the debuggee what a
bare run would have given it**. not always a terminal, and not always a pipe —
the same thing

it matters because `isatty()` is not cosmetic. it is what `rich`, `click`,
`pytest` and `colorama` check to decide colour, progress bars and formatting, so
a program told the wrong answer *renders* differently. the buffering follows
from it and is the smaller half: cpython line-buffers a terminal and
block-buffers a pipe, so on a pipe an unflushed `print` waits for the program to
exit or for the buffer to fill. how big that buffer is is not a constant worth
quoting — cpython sizes `sys.stdout` from the descriptor's own `st_blksize`, so
it differs by platform and by what the descriptor is

### `bpd launch` inherits, and that is the whole mechanism

`bpd launch` gives the debuggee no streams of its own. the interpreter is spawned
with stdin, stdout and stderr inherited, so the debuggee is holding **the same
file descriptors** `bpd` was handed — not an imitation of them. measured, as
`sys.stdout`:

|                             | `isatty()` | `line_buffering` |
| --------------------------- | ---------- | ---------------- |
| bare, in a terminal         | `True`     | `True`           |
| `bpd launch`, in a terminal | `True`     | `True`           |
| bare, redirected            | `False`    | `False`          |
| `bpd launch`, redirected    | `False`    | `False`          |

a pseudo-terminal here would be strictly worse. it would be a *second* terminal
between `bpd` and the real one, and then the size, the window-change signal and
the line discipline would all be `bpd`'s to keep in step with a terminal it is
no longer connected to — and stdout and stderr, which are genuinely two
descriptors on a bare run, would arrive on one

### `bpd dap` and `bpd mcp` capture, and give a pipe

on those two the debuggee's output cannot be left where it is: `bpd`'s own stdout
**is the protocol**, and one `print` from the program in the middle of a message
makes every message after it unreadable. so the program's stdout and stderr are
pipes, read by a thread each, and forwarded — as DAP `output` events with a
`stdout` or `stderr` category, and as the same distinction over MCP

that is what a launch does when the front end is the thing reading the program's
output. the one launch where it is not is a DAP client asking for a terminal of
its own, and then there are no pipes and no forwarding at all —
[below](#runinterminal-the-client-owns-the-terminal-and-starts-the-program)

that leaves a judgement to make, because there is no bare run to copy: what is
the debuggee's output going *to*. it is going to a program — the DAP client —
which reads it and renders it in a debug console. `python program.py | client`
is the bare equivalent of that, and it is a pipe. so a pipe is what the rule
asks for, and the block buffering that comes with it is the buffering a bare
piped run has. a debuggee under `bpd dap` whose `print` has no `flush=True`
reaches the console when the program exits, and would have done the same without
`bpd`

a pseudo-terminal was considered for this path and **refused**, for four reasons
that are all the same reason:

- `isatty()` would return `True`, which is a claim that there is a terminal.
    there is not. a debug console does not deliver keystrokes to the program,
    has no size, and does not act on cursor motion — so a program told it has a
    terminal writes colour, `\r` progress redraws and cursor escapes into a
    widget that shows them literally. that is `bpd` making the program's output
    *wrong* in order to make it *timely*
- a terminal has a size, and this one would not have a real one.
    `os.get_terminal_size()` on a fresh pseudo-terminal is `0x0` until somebody
    sets one, and setting `80x24` is a number nobody measured. a debugger
    inventing a fact about the program's world is the thing this project exists
    not to do
- a terminal has **one** stream. stdout and stderr would arrive merged, and
    which one a line came from — a real fact about the program, reported today
    as the DAP `output` category — would be gone
- it fixes the timeliness of output by changing what the program *is*, and DAP
    already has the answer for a program that genuinely needs a terminal:
    `runInTerminal`, where the client owns the terminal and makes it. that is
    why the request exists, and `bpd` implements it — see below

### a program is not reported over until what it wrote has been carried

a pipe read on a thread introduces a second channel, and the program being over
arrives on neither of them. it arrives on the **control connection** — the agent's
socket, which closes when the process dies — and nothing orders that against the
pipes. so a forwarding thread that has not caught up yet is a client told the
program finished while its last lines are still in a pipe

that is not cosmetic. an `exited` event is what a client uses to decide the run
is done: to stop tailing the console, to render a result, to tear the session
down. a line arriving after it is a line that reaches nobody, and `bpd` claims a
run under it is indistinguishable from a bare one — a bare run does not lose its
last line

measured, on a program printing four thousand lines and exiting: the client had
**3665 of them** when it was told the program had exited. the rest arrived
afterwards. a pipe holds about 64 KiB, so any program that writes more than that
before ending has this window by construction, and the size of it is the size of
whatever is left unread

so the engine waits for the forwarding before it reports the exit. it is
`bpd_engine::Forwarders`, handed over by whichever front end made the threads,
and it is waited for in one place rather than in each of them — a rule kept by
the thing that knows the program is over cannot be forgotten by a front end
added later

the wait is **bounded**, and that bound is the interesting part. a forwarding
thread ends at end-of-file, and end-of-file needs every write end of the pipe
closed — but a **forked child inherits one**. a program that leaves a child
running never reaches end-of-file at all, so an unbounded wait here would turn
"the program exited" into a hang for exactly the programs a debugger following
children exists for

what happens instead is that the wait gives up after two seconds and **says so**:

- the CLI never sees it, and asserts as much — `bpd launch` inherits the
    streams, so there is no pipe of `bpd`'s to drain and the order is the
    kernel's
- **DAP** writes a console line before the `exited` event, saying the program
    has exited and something that outlived it still holds the stream
- **MCP** carries `output_complete` on every exit — present whether or not
    anything is wrong, because a field that appears only on failure cannot be
    told from a server that does not report it — and a `note` giving the reason
    when it is `false`

what each of them says is that the wait did not finish, not *why* it did not.
end-of-file needs every write end closed and a surviving child is what usually
holds one — but `bpd` did not watch the descriptor, and naming a cause it did
not see would be the invention the line exists to avoid

nothing is dropped in that case either. whatever still holds the stream keeps
being read and forwarded; what is lost is only the *claim* that a line was
written before the program ended, and that claim is what is withdrawn rather
than quietly kept

### the same two give the debuggee no stdin at all

a captured launch gives the debuggee `/dev/null` as its stdin. it used to give
it `bpd`'s own, and that was not a smaller version of the output problem — it
was a worse one:

- over **stdio** — `bpd dap` with no flag, and `bpd mcp`, which has no other
    transport — `bpd`'s stdin *is* the protocol. the debuggee and the adapter
    were two readers of one descriptor, so a program calling `input()` did not
    merely hang: it took the client's next message out of the stream. measured,
    it returned `'Content-Length: 68\r'`, and the request those bytes belonged
    to was answered by nothing. the session was **corrupted** rather than
    stalled
- under **`--listen`** the protocol is on the socket, so there was nothing there
    to corrupt — and the stream was still not the debuggee's. it is whatever
    spawned `bpd`: an editor, a script, a CI job. measured, a debuggee read a
    line out of a file its launcher was reading

the rule is the one the output follows, and it is about the **capture** rather
than the transport: a front end that has taken the program's output over has
taken `bpd`'s streams for itself, and there is no bare run left to inherit from.
so the debuggee gets what `python program.py < /dev/null` gives, on all three
paths — `bpd dap` on stdio, `bpd dap --listen`, and `bpd mcp`

that is a defined outcome rather than a hang. `input()` raises **`EOFError`**,
`sys.stdin.read()` returns `''`, and `sys.stdin.isatty()` is `False` — at the
line that asked for it, where the debugger stops on it like any other exception.
it is a real stream object, and that is the reason for `/dev/null` rather than a
closed descriptor: with descriptor 0 not open, cpython sets `sys.stdin` to `None`
and `input()` raises `RuntimeError: input(): lost sys.stdin` instead, and the
next file the program opened would be handed descriptor 0

`--listen` inheriting was considered separately, because it is the one path
where `bpd`'s stdin is not the protocol. it is refused for two reasons: the
stream still belongs to another reader, and taking bytes from it is the same
theft with a quieter victim — and it would make the **program** behave
differently depending on which socket its client connected over, which is one
adapter giving one program two answers about its own world

a program that genuinely needs input has one honest route, and it is DAP's:
`runInTerminal`, where the client starts the program in a terminal it owns and
the terminal is a real one. that is the next section, and it is why no channel
was invented here — a pipe the adapter wrote to would be a debugger delivering
keystrokes to a program that has no terminal to receive them at, and it would be
a capability in one adapter and not the other. under `bpd mcp`, which has no
such request and no terminal on either side, a program still takes its input
from a file, an argument or the environment; that gap is
[written down](dap.md#the-parity-rule-both-sided) rather than left to be noticed

`a_program_that_reads_its_stdin_gets_an_empty_one_rather_than_bpds` in
`crates/bpd/tests/dap.rs` runs on **both** transports and pins two of the three,
and `..._rather_than_the_servers` in `crates/bpd/tests/mcp.rs` pins the last. the
loopback half is not vacuous: the listening adapter is started with a marked line
in its own stdin, so a debuggee that inherited it reads the marker and the
assertion says what was taken

### `runInTerminal`: the client owns the terminal, and starts the program

a DAP client asks for it with `"console": "integratedTerminal"` or
`"externalTerminal"` in its launch configuration, and then the debuggee is on a
**real** terminal: `isatty()` is `True`, the buffering is a terminal's, and
`input()` returns the line somebody typed. it is the only route to either under
this adapter, and the reason it works when a pseudo-terminal of `bpd`'s would
not is that the terminal is one the client already **has**

what changes is the last step of a launch and nothing before it. the interpreter
is probed, the source map is loaded, the agent is staged, the listener is bound
and the environment is written exactly as they are for a spawn — and then,
instead of `Command::spawn`, the argument vector and the environment go to the
client in the reverse request. the agent connects **back** to the engine from
the terminal, which is the same connection a launched debuggee makes, so
everything after the handshake is one implementation:

```text
bpd                                        the client              a terminal
 │
 ├─ probe, map, stage, bind, write the environment
 ├─ runInTerminal ────────────────────────▶ │
 │                                          ├─ opens one ─────────────▶ │
 │                                          ├─ runs the argument vector ┤
 │  ◀──────────── the response ─────────────┤                           │
 │                                                                      │
 │  ◀──────────── connect, handshake ───────────── python -c "import bpd_agent…"
 │  ◀──────────── stopped: entry ──────────────────┤
 └─ the launch is answered
```

the answer to the reverse request is **waited for**, and that is the difference
between a refusal and a hang: a client that could not run the command line says
so, and the launch is refused with what it said. a client that answers nothing
at all is given thirty seconds and then the same refusal with that as the reason

three things follow from bpd not being the parent of that process, and each is
reported rather than papered over:

- **the program's output does not reach the debugger at all.** there are no
    pipes to read: the terminal is the program's stdout and stderr, and the
    client is the thing that owns it. so no `output` event carries a line of the
    program's on this path, and nothing claims to. that is the trade — a debug
    console gets the program's output and not a terminal, a terminal gets the
    program its own streams and the debugger sees none of it
- **there is no exit code.** the program ends, the control connection closes,
    and what it exited with is not `bpd`'s to read. that is `Running::Ended`,
    which already existed for a session that arrived on the listener, and DAP
    gets `terminated` with deliberately **no** `exited` — the event carries an
    `exitCode` as a required field and a zero would be invented
- **`bpd` cannot end it.** ending a debuggee is signalling the child `bpd`
    holds and reaping it, and there is no child. so `disconnect` is answered and
    the refusal — "bpd did not start that process and is not its parent" — goes
    to the client on the `console` category rather than being swallowed, which
    is the shape a client reads as a program that has been stopped. what does
    end it is the agent: the control connection goes when the adapter does, and
    the agent's own rule is that it exits the program rather than carrying it on
    with no debugger attached — on its own stderr, which here is the terminal
    the person is looking at

a program that fails to start is the other side of the same fact. a
`SyntaxError` is printed by the interpreter **in the client's terminal**, and
what `bpd` can say is that the agent connected and the program never reached its
first statement — so the refusal says exactly that, and says where the words
are. there is no `ExitedBeforeStopping` here, because that carries an exit
status and there is none

`a_program_the_client_starts_in_a_terminal_really_has_one` in
`crates/bpd/tests/dap.rs` is the whole of it, on **both** transports: a real
`bpd dap`, a real pseudo-terminal the test client opens, the argument vector run
in it exactly as it arrived, `isatty` read off the program's own output, and a
line typed at the terminal that the program reads with `input()`. the client
capability is checked by `a_terminal_is_refused_when_the_client_never_said_it_had_one`,
which is the refusal at `launch` — before an interpreter is probed, an agent is
staged or a port is bound

#### it is refused when the client cannot be asked

`supportsRunInTerminalRequest` is a **client** capability, in `initialize`'s
arguments, and it is the second one this adapter reads —
`supportsStartDebuggingRequest` being the first. a client that has not
advertised it cannot be asked to start anything, so a launch asking for a
terminal would wait for an agent nobody was going to start. it is refused at
`launch`, by name, with what the program gets instead. that is the same shape as
`debugChildren`'s refusal and for the same reason: the only moment at which
refusing costs nothing is before anything has happened

### the parity test could not see any of this, and now can

`crates/bpd/tests/launch_parity.rs` runs everything twice and compares, and every
comparison in it went through `Command::output` — which is two pipes on both
sides. so anything differing only between a terminal and a pipe was invisible to
it: a change that put a pipe in front of the debuggee would have passed the
whole file

`a_program_on_a_terminal_is_still_on_one_under_bpd` is the comparison it could
not make. it opens a real pseudo-terminal, gives it to a bare interpreter and to
`bpd launch` as all three standard streams, and compares what came back. the
probe reports `isatty` on each stream and then writes one line with `os.write`,
straight to the file descriptor and past python's buffer — so *where that line
lands* among the ones before it is the buffering, measured rather than asked
about

it writes to **both** streams, which is the second thing only a terminal can
settle. `output_arrives_in_the_same_order_on_both_streams` promises more than it
checks: it compares each stream against its own counterpart, and two pipes carry
no record of how the two were interleaved, so nothing there can see across them.
a terminal has one stream, and the order the two arrive in on it is the order
the program wrote them — which is also the fact that makes the merging above a
real cost rather than a tidy-up

it carries its own guard. the same probe is run through pipes, and the test fails
if the two shapes ever stop differing — otherwise it would be the piped
comparison a second time, passing while proving nothing

it is unix-only, and says so by being `#[cfg(unix)]` rather than by skipping.
windows has no pseudo-terminal of this shape — ConPTY is a different mechanism
with a different API — and there is nothing on windows for the test to be about,
because `bpd launch` inherits its streams there exactly as it does here

## the one fingerprint that remains

`bpd_agent` stays in the debuggee's `sys.modules`. it cannot be removed —
unimporting it would unload the code that is running

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

### what a program can still tell, in full

a program can list its own `sys.modules`, and what is in there under `bpd` is
not quite what is in there without it. that matters beyond curiosity: a plugin
scanner, a lazy importer, or a test that asserts on an import side effect
behaves **differently** because a module it did not import is already there

so the difference is measured rather than described. this is the delta against a
bare run of the same program — not the whole of what the agent touches, since
`sys`, `builtins`, `os`, `_thread` and `_imp` are in every interpreter before the
agent exists and are not a difference at all:

|       | script     | `-m`       | `-c`       |
| ----- | ---------- | ---------- | ---------- |
| 3.13  | 31 → **2** | 26 → **2** | 26 → **1** |
| 3.14  | 32 → **2** | 26 → **2** | 30 → **1** |
| 3.14t | 32 → **2** | 26 → **2** | 30 → **1** |
| 3.15  | 32 → **2** | 26 → **2** | 30 → **1** |

measured on macOS, where two of the names it was are `_osx_support` and
`_sysconfigdata__darwin_darwin` — the left column is a little platform's own.
the right one is not: the same **two names**, on every interpreter and every
platform, and the same reason for each:

- **`bpd_agent`**, in all three forms. importing the agent costs exactly this one
    name: it is a `cdylib` with no python of its own, so nothing comes with it.
    it stays because unimporting it would unload the code that is running
- **`linecache`**, in the script and `-m` forms. this one is **cpython's, not
    the agent's**: every `-c` run imports it to keep the command's source where a
    traceback can find it, and `bpd` enters all three forms through a `-c`
    bootstrap. it is in `sys.modules` before the agent is imported at all, and a
    bare `-c` run has it too — which is why the command form's delta is one name
    rather than two

what the rest were is worth writing down, because it is the shape this mistake
takes. **twenty-nine of the thirty-two were one call** — twenty-five of 3.13's
thirty-one, the same call costing a little less there:
`sysconfig.get_config_var("Py_GIL_DISABLED")`, which the agent asked once to tell
a free-threaded interpreter from a gil one. importing `sysconfig` is eight names
— `threading` and `types` among them — and the call is twenty-one more, because
it loads `_sysconfigdata_…` and `_osx_support`, and `_osx_support` imports `re`,
which brings `enum`, `functools`, `collections`, `operator`, `copyreg`,
`reprlib`, `keyword` and five `re` submodules. one lookup, a fifth of the stdlib

it is read off the extension suffix instead. `_imp.extension_suffixes()[0]` is
`EXT_SUFFIX`, built from `SOABI`, and the interpreter tag in it carries the `t`
exactly when `Py_GIL_DISABLED` was set — `cpython-314t-darwin` against
`cpython-314-darwin`, `cp314t` against `cp314` on windows. `_imp` is a builtin
every interpreter has already imported. a suffix carrying neither `3xx` nor
`3xxt` is an error naming the suffix, not a guess

`sys._is_gil_enabled()` and `sys.flags.gil` were both rejected, and the reason is
not taste. a free-threaded build turns the gil **back on** when it imports an
extension that has not declared itself free-threading safe — which is exactly
what a mismatched agent is — so both would report a free-threaded interpreter as
a gil one in the one case the check exists for. `sys.abiflags` carries the same
`t`, and is documented `Availability: Unix`

two imports of the agent's own outlived that one, and they are the same kind of
thing at a smaller size — a module imported to reach a single name that was
already in the process. `types` was imported for
`types.ModuleType`, which `Lib/types.py` defines as `type(sys)` and nothing else,
so the fresh `__main__` is built from `type(sys)` directly. and
`importlib.machinery` was imported for `SourceFileLoader`, which is reached
instead through the frozen `importlib._bootstrap_external` the interpreter is
already running on — the same route `set_main_loader` in `pythonrun.c` takes, and
the same class object, without the four `sys.modules` entries, two of which are
aliases for modules the interpreter has already loaded

### why nothing is deleted from `sys.modules`

the shorter way to make this table read zero is to take the names back out. it is
not on the table, and the reason is measured rather than assumed. deleting a
module does not undo the import — it makes the **next** import of it run the
module's top level a second time, in a fresh namespace:

| after `del sys.modules[…]` and importing again |                                                                                   |
| ---------------------------------------------- | --------------------------------------------------------------------------------- |
| the module's top level                         | runs a second time                                                                |
| its classes                                    | are new objects — `isinstance` against the old ones is false                      |
| `threading.main_thread()`                      | a different object, and the real main thread is not in the new registry           |
| `enum.Enum`                                    | a different class, so classes built on the old one are no longer subclasses of it |

`re` looks like it survives — a compiled pattern is still an `re.Pattern`
afterwards — but only because `re.Pattern` is a C type from `_sre` rather than
one of `re`'s own. that is luck, not a rule

so hiding `threading` this way would hand a program that touches threads a thread
registry that does not contain its own main thread. a module with a side effect
on its top level would perform it twice. that is a *behaviour* change, and it
would be made to conceal a cosmetic one

### the list is a test, not a note

`the_only_modules_a_debuggee_gains_are_the_ones_written_down` in
`crates/bpd/tests/launch_parity.rs` runs a program that prints its own
`sys.modules` bare and under `bpd`, in all three forms, and compares. the two
names above are written down there **with their reasons**, and anything else
appearing fails the test with the name it found and the question of whether it
should have

it fails in the other direction too: a name in the list that no form produces any
more is a reason nobody needs, and the test says so. a list without reasons is a
list people update instead of reading

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
