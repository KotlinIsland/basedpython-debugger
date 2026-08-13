# the intellij plugin

`editors/intellij/` is a **registration**, the same way
[the vs code extension](vscode.md) is, and deliberately nothing else

a jetbrains IDE resolves a debug session through a run configuration type and a
`ProgramRunner`, neither of which can be named from a settings file. so without
a plugin no pycharm user could run `bpd` however complete
[the adapter](dap.md) is

what it contributes is a run configuration, the attributes it carries, and the
answer to "where is `bpd`". everything after that is DAP, and `bpd dap` already
speaks it

## what it is built on

the platform has a **general DAP layer**, `com.intellij.platform.dap`, over
`org.eclipse.lsp4j.debug`. it is not the python plugin's — jetbrains' own
[Python DAP Debugger](https://plugins.jetbrains.com/plugin/28460-python-dap-debugger)
is a *client* of it, registering `platform.dap.debugAdapterSupportProvider` for
debugpy. `bpd` registers the same extension point with an adapter id of its own,
beside it rather than inside it

the layer's module descriptor says `visibility="public"` and its two extension
points are `dynamic="true"`. the classes carry **`@ApiStatus.Experimental`** —
not `Internal`, which is what had been feared. that is read off the artifact,
because none of it is documented

### which IDEs have it

**this is the constraint the whole plugin sits under, and it is not written
down anywhere:**

| IDE (2026.2.1)                 | `intellij.platform.dap` |
| ------------------------------ | ----------------------- |
| PyCharm (unified, `PY`)        | yes                     |
| IntelliJ IDEA Ultimate (`IU`)  | yes                     |
| PyCharm Community (`PC`)       | **no**                  |
| IntelliJ IDEA Community (`IC`) | **no**                  |

so the plugin targets the **unified PyCharm**, which is what a python user
installs now — pycharm community was discontinued as a download after 2025.3 and
survives only as a maven artifact for plugin developers. `plugin.xml` declares
`<module name="intellij.platform.dap"/>`, so an IDE without the layer declines
to load the plugin rather than loading it and failing at the first session

## a run configuration

`bpd` contributes a run configuration type of its own — not a **Mode** beside
jetbrains' debugpy backend, because that switch belongs to the python plugin and
selects between *its* two backends. what a `bpd` configuration holds:

| field            | what it is                                                              |
| ---------------- | ----------------------------------------------------------------------- |
| script           | the program to run                                                      |
| arguments        | arguments for the program, exactly as it receives them                  |
| interpreter      | the interpreter to run under, resolved on `PATH` like any other command |
| `bpd executable` | the `bpd` to start — see below                                          |
| stop on entry    | stay stopped before the program's first statement                       |
| stop the world   | hold every thread that can be held for the duration of each stop        |
| debug children   | debug a child the program forks                                         |

each is sent in the `launch` request under the name
[the adapter reads it by](dap.md#what-a-configuration-can-say). what is **not**
offered is `variables` and `threadSettleMs`: the adapter's defaults apply and
there is no field for them, which is an absence rather than a control that does
nothing

there is no Run button. bpd has no path that launches a program without its
agent — DAP calls that `noDebug` and the adapter refuses it by name — so the
configuration is `RunConfigurationWithSuppressedDefaultRunAction` and only the
debug executor offers it

breakpoints are the python plugin's own `PyLineBreakpointType` and
`PyExceptionBreakpointType`, which are the breakpoints a person has already set.
a second line breakpoint type for the same `.py` file would put two of them in
one gutter and make which one a click produces a matter of priority. jetbrains'
debugpy backend names the same two

## where `bpd` is found

the rule is [the vs code extension's](vscode.md#where-bpd-is-found), copied
field for field:

| `bpd executable`           | what happens                                                    |
| -------------------------- | --------------------------------------------------------------- |
| unset — the default, `bpd` | looked up on `PATH`, entry by entry                             |
| a bare name                | the same lookup, for that name                                  |
| an absolute path           | used as given, if there is a file there this machine would run  |
| a relative path            | **refused** — it would resolve against a directory nobody chose |
| empty                      | **refused**, naming the field                                   |

the `PATH` walk is done by the plugin rather than left to the process spawn, and
that is the whole point of it. it is checked twice: `checkConfiguration` runs it
when the run configuration dialog validates, which is the moment the person is
looking at the field that is wrong, and `launchDebugAdapter` runs it again
before starting anything

on windows the lookup uses `PATHEXT` and the presence of the file, because that
is what windows goes by; everywhere else it checks the executable bit

## the transport

the plugin starts **`bpd dap --listen 0`** and connects a socket to it, rather
than using the platform's `CommandLineDebugAdapterHandle`, which would have been
one line and speaks to an adapter on its pipes

the reason is `debugChildren`. DAP hands a second program to a client with the
`startDebugging` reverse request, which asks the client to open a **second
connection** — and nothing can open a second connection to a pair of pipes
somebody else spawned. the adapter refuses `debugChildren` by name on that
transport, and the socket is what keeps the refusal from being permanent

`--listen 0` binds a port the operating system chooses, and prints one line of
json on stdout before it accepts anything:

```json
{
  "listening": {
    "host": "127.0.0.1",
    "port": 54321,
    "header": "x-bpd-token",
    "token": "…"
  }
}
```

the token is required on the **first message**, and there is no way to ask the
platform's lsp4j launcher to add a header. so the plugin writes it onto the
socket itself, before the launcher writes anything: a DAP header block is
`Name: value` lines terminated by a blank line and `Content-Length` was never
required to be the first of them, so `X-Bpd-Token` on the line above it is one
header block with two headers rather than two messages

## the console

**the program's own output reaches the IDE console, and the platform's default
is what puts it there.** the plugin overrides nothing for it

that was the open question, and it was open for a reason:
[`basedpython-pycharm`](basedpython-pycharm.md) had to override
`createXDebugProcess` because `DapXDebugProcess` builds a console over its own
process handler, and an adapter that spawns the debuggee itself — which `bpd`
does — is not the debuggee's process. what the platform actually does, read off
the artifact and then driven:

- `DapXDebugProcess.createConsole` builds a `TerminalExecutionConsole` over
    `XDebugProcess.getProcessHandler`, which for a DAP session is the platform's
    own `DefaultDebugProcessHandler` — a handler with no process behind it. so
    the console is **not** a view of anything the adapter's process wrote, and
    the fact that `bpd` owns the debuggee costs nothing
- `DapXDebugProcess.launchOutputListener` drains the DAP session's `output`
    events and `formatAndPrintOutput` prints each into
    `XDebugSession.getConsoleView`, mapping the category: `stderr` to
    `ProcessOutputType.STDERR`, `console` to `SYSTEM`, everything else — which
    includes `stdout` — to `STDOUT`
- `bpd_dap` sends the debuggee's two streams as `output` events under exactly
    `stdout` and `stderr` (`Console::wrote`), so both land as themselves

`basedpython-pycharm` also drops adapter `output` events on the floor, because
debugpy opens every session with two bare events reading `ptvsd` and `debugpy`.
that filtering would be wrong here: **`bpd` sends no `output` event of its own**.
`bpd dap --listen 0` announces its port as one line of json on its **stdout**,
and `BpdConnection` reads that line off the pipe — it is not the debuggee's
output and never becomes an event. the console holds the program's lines and
nothing else, and that is asserted rather than assumed

### stdout is buffered, and that is cpython

`bpd` hands the interpreter a pipe rather than a terminal, and cpython
block-buffers a stdout that is a pipe. so a `print` that is not flushed sits in
the interpreter and arrives at the console when the program exits — measured
against the adapter directly, not inferred: at a breakpoint the client has the
program's `stderr` and not its `stdout`, and both `stdout` lines arrive together
just before `exited`

stderr is line-buffered whatever it is attached to, so it is always there

this is the adapter's behaviour rather than the plugin's, and it is the same in
vs code. `program.py` flushes its one stdout line for that reason, so the suite
reads both streams at the breakpoint rather than waiting for a program to end —
which is also the moment a person is actually reading the console

## what the platform does with bpd's capabilities

three things were unknown before this was built, and driving it answered two:

- **`supportsSingleThreadExecutionRequests` is honoured.** bpd's default is
    non-stop — a stop holds one thread and the rest of the program keeps
    running — and debugpy does not advertise that capability at all, so the
    platform layer had never had to. it does: `DapDebugSessionImpl.resume` reads
    the capability off the `initialize` response and sets
    `ContinueArguments.singleThread` from it, and the stepping path sets it
    unconditionally. that is read out of the bytecode, and it means a `bpd`
    session in pycharm resumes the way it does in vs code
- **`startDebugging` is not advertised.** the platform's default
    `createInitializeParams` sets `clientID`, `clientName`, `pathFormat`,
    `locale`, `supportsVariableType`, `supportsVariablePaging`, `linesStartAt1`
    and `columnsStartAt1` — and nothing else. `DapClient` does not override
    lsp4j's `startDebugging` either. so **`debug children` is refused, by name**,
    with the adapter's own sentence: it is a refusal a person can read rather
    than a fork that vanishes
- **custom requests have a route, and it is not taken.** `bpd/replaceCode` and
    `bpd/runScript` are not methods of lsp4j's `IDebugProtocolServer`, and
    `CommandScope.server` is typed as one — but `DebugAdapterDescriptor` has a
    `debugAdapterServerClass`, and jetbrains' own plugin uses it: it declares
    `PythonDapProtocolServer : IDebugProtocolServer` with `@JsonRequest`
    methods for `getTable`, `getArray`, `getTableImage` and
    `setDebuggerProperty`, and the layer proxies the interface it is given. so
    an interface with `@JsonRequest("bpd/replaceCode")` on it would work.
    nothing here declares one, because nothing would call it: reaching either
    from an IDE means an action, and an action is UI this plugin does not
    contribute. both stay reachable from [the MCP server](mcp.md) against the
    same session

## driving it

a real pycharm, downloaded by the build, starting a real session through this
plugin. that is what `editors/intellij/src/test/` is, and it is the reason the
intellij MVP criterion is ticked

```sh
cargo build -p bpd_agent
cargo build --bin bpd
cd editors/intellij
BPD_EXECUTABLE="$PWD/../../target/debug/bpd" BPD_PYTHON=python3.14 ./gradlew test
```

`BPD_PYTHON` has to name the **same interpreter the agent was built against** —
the agent is a cpython extension and is not abi3, so an agent built for one
interpreter will not import into another. the suite asks `bpd doctor` about it
before a session is started, so an interpreter that could not run one is refused
by name rather than at the far end of a session that fails to start

`intellij-platform-gradle-plugin` downloads the IDE itself, so nothing has to be
installed. it lands in the gradle cache rather than in the checkout

what one run does: writes `program.py` into a project on disk, puts a breakpoint
on the line holding the marker comment through the IDE's own
`XBreakpointManager`, starts a `bpd` run configuration under the debug executor,
waits for `XDebuggerManager` to hold a paused session, reads the frame the IDE
focused and a local out of its variables view, reads the console the IDE built,
resumes with `XDebugSession`, and waits for the program to write its last file

**the evidence is taken from the IDE, not from bpd.** whether the adapter
answers a `stackTrace` correctly is settled by the rust suite, and re-reading
bpd's output here would prove that again rather than the thing in question. so
what is asserted is the IDE's own debug state — that the platform loaded the
plugin's extension point registrations, that `XDebuggerManager` holds a session,
that the session is paused, that the frame it focused is in the file the
breakpoint is in and on its line, that the variables view says `total` is `3`,
and that the terminal buffer of `XDebugSession.getConsoleView` holds the two
lines the program wrote and no line it did not. the program writes a file as its
last statement, which is how "the session ended because the program exited" is
told apart from "the session was killed"

nothing waits for a duration. every step waits for a **condition** with a
deadline, and the deadline's message names the thing that never arrived — a
console assertion that slept instead would pass on a machine that is quick and
fail on one that is loaded, which is worse than not asserting it

`program.py` is `editors/vscode/test/program.py` with two `print` calls added,
and that difference is deliberate: a program that writes nothing cannot say
whether what it writes reaches a console. the vs code suite does not assert on
its console, so its copy has nothing to gain from them

it runs **headless**. the platform test framework is not an electron app and
wants no display, so unlike the `vscode` job there is no `xvfb` in CI

## what it does not do

no panels, no views, no tree, no tool window, no inline value renderer, no
actions. this is the line `ROADMAP.md` draws

more specifically, and for reasons that are the adapter's rather than the
plugin's:

- **no attach configuration.** attaching is PEP 768 and needs cpython 3.14, and
    it is not built. `bpd_dap` refuses an `attach` request by name
- **no interpreter discovery.** the interpreter is a command resolved on `PATH`,
    the same way `bpd launch --python` resolves one. the plugin does not ask the
    python plugin which SDK the project has — an interpreter the debugger chose
    for you is an interpreter you did not choose. it is also what makes the
    plugin work in a project with no python SDK configured at all
- **no run configuration producer.** there is no "debug this file with bpd" in
    the editor's context menu; a configuration is made the ordinary way

## what was checked, and what was not

**checked by driving a real pycharm**, in `editors/intellij/src/test/` — the
list above, plus both refusals: a `bpd executable` naming a file that is not
there is refused by `checkConfiguration` with the path and the remedy in the
sentence, and a relative one is refused rather than resolved

the console assertion was negative-checked the way the rest of the suite was:
with `formatAndPrintOutput` overridden to do nothing — which is exactly the
platform behaviour it is about — the session still stops on the breakpoint and
still reads `total` as `3`, and the run fails with `the program's stdout
reaching the IDE console did not happen`

**checked mechanically, in `cargo test`** — `crates/bpd_dap/tests/intellij.rs`:

- every attribute the plugin sends in its `launch` request is one
    `bpd_dap::Configuration` reads, with the field names taken from serde rather
    than from a copy of them. this matters more here than it does for vs code:
    `Configuration` deliberately does **not** deny unknown fields, because a DAP
    `launch` carries the client's own keys — so a misspelled attribute is not an
    error, it is a setting a person fills in and never gets, and nothing else
    would ever say so
- every attribute the adapter reads is either sent or excused by name, and each
    excuse carries its reason
- every attribute sent is stored on the run configuration and both written and
    read back by the settings editor, so none is a constant wearing a
    configuration and none is a field nobody can reach
- the manifest depends on `intellij.platform.dap` and on python support, the
    adapter id is still `bpd`, and the plugin still starts
    `bpd dap --listen 0`

**not checked.** the suite drives one platform per run, and CI drives linux, so
the windows `PATHEXT` branch of the lookup has no coverage. `debug children` is
sent in the launch request and the adapter's refusal is not driven from here —
it is driven in the rust suite. the plugin is not published to the marketplace
and the repository does not build a signed zip; `./gradlew buildPlugin` produces
an unsigned one
