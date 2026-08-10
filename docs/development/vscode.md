# the vs code extension

`editors/vscode/` is a **registration**, and deliberately nothing else

vs code resolves a launch configuration's `"type"` through an extension that
contributes a `debuggers` entry. there is no way to name an adapter executable
from `launch.json` alone — so without this, no vs code user could run `bpd`
however complete [the adapter](dap.md) is. `nvim-dap` names the executable in
its own configuration and has driven the same adapter since it was built, which
is what makes this a vs code problem rather than an adapter one

what it contributes is the type, the launch attributes a `launch.json` may hold,
and the answer to "where is `bpd`". everything after that is DAP, and `bpd dap`
already speaks it

## installing it

nothing is published to the marketplace, and the repository does not build a
`.vsix`. what there is, is a directory vs code can load as it stands — the
extension is plain commonjs javascript with no dependencies and no build step,
which is the reason it is javascript rather than typescript

```sh
ln -s "$PWD/editors/vscode" ~/.vscode/extensions/bpd
```

then reload the window. `~/.vscode-insiders/extensions` for insiders, and
`%USERPROFILE%\.vscode\extensions` on windows, where a copy is easier than a
link

`bpd` itself is a separate thing to have: the extension starts `bpd dap`, so
there has to be a `bpd` to start. see below for how it is found

## a launch configuration

```json
{
  "version": "0.2.0",
  "configurations": [
    {
      "type": "bpd",
      "request": "launch",
      "name": "debug this file with bpd",
      "program": "${file}",
      "python": "python3.14",
      "stopOnEntry": false
    }
  ]
}
```

every attribute is in [what a configuration can say](dap.md#what-a-configuration-can-say),
and every attribute there is one the adapter reads. the two lists are the same
list, and that is checked rather than promised —
`crates/bpd_dap/tests/vscode.rs` reads `package.json` off disk and compares its
schema against `bpd_dap::Configuration`, field by field, taking the field names
from serde rather than from a copy of them. an attribute added to one side and
not the other fails the rust test suite, including the declared defaults and the
bounds inside `variables`

## where `bpd` is found

a hard-coded path would be a lie on someone else's machine, so it is a lookup:

| `bpd.executable` | what happens |
| --- | --- |
| unset — the default, `bpd` | looked up on `PATH`, entry by entry |
| a bare name | the same lookup, for that name |
| an absolute path | used as given, if there is a file there this machine would run |
| a relative path | **refused** — it would resolve against a directory nobody chose |
| empty | **refused**, naming the setting |

the `PATH` walk is done by the extension rather than left to the process spawn,
and that is the whole point of it: a spawn that cannot find its program produces
"the debug adapter exited unexpectedly", which says nothing a user can act on.
what they get instead names the command that was not found, says that `bpd` is
its own adapter and is run as `bpd dap`, says how many directories of `PATH`
were searched, and gives the two ways to fix it — put it on `PATH`, or set
`bpd.executable` to the absolute path

on windows the lookup uses `PATHEXT` and the presence of the file, because that
is what windows goes by; everywhere else it checks the executable bit

the adapter is started with the session's workspace folder as its working
directory. the debuggee inherits it and a program can see it, so it is the
folder the configuration came from rather than whatever directory vs code
itself was started in

## what it does not do

no panels, no views, no tree, no status bar item, no inline value renderer, no
commands. this is the line `ROADMAP.md` draws, and it is drawn where it is
because everything on that list is a second implementation of something DAP
already carries

more specifically, and for reasons that are the adapter's rather than the
extension's:

- **no `attach` configuration.** attaching is PEP 768 and needs cpython 3.14,
    and it is not built. `bpd_dap` refuses an `attach` request by name, so a
    schema that completed `"request": "attach"` would offer an affordance whose
    only outcome is a refusal
- **no restart affordance**, for the same reason — there is no restart behind it
- **"run without debugging" is refused.** vs code sends `noDebug` for it, and
    `bpd` has no path that launches a program without its agent. running it
    anyway would debug a program the user asked not to debug, so the adapter
    refuses and says to run it without `bpd`
- **no interpreter discovery.** `python` is a command resolved on `PATH`, the
    same way `bpd launch --python` resolves one. the extension does not ask the
    python extension which environment is selected, and does not read a
    `.venv` — an interpreter the debugger chose for you is an interpreter you
    did not choose
- **no dynamic configuration provider.** the extension contributes what a new
    `launch.json` starts as and what completion offers inside one; it does not
    synthesise a configuration for a file with no `launch.json` at all

## what was checked, and what was not

this matters more than usual here, because the rust test suite cannot drive vs
code and a `package.json` that parses is not evidence that an extension works

**checked mechanically, in `cargo test`** — `crates/bpd_dap/tests/vscode.rs`:

- every attribute the schema contributes is one `bpd_dap::Configuration` reads,
    and every field it reads is contributed. `noDebug` is the single hand
    written exception and it carries its reason
- every declared type and default is the one the adapter really uses, taken
    from a parsed `Configuration` rather than from a list beside it
- the `variables` sub-schema is exactly `bpd_core::Detail`'s fields, with
    `additionalProperties: false` because `Detail` denies unknown ones
- `configurationAttributes` offers `launch` and nothing else
- every snippet and initial configuration writes only attributes that exist
- the manifest and the javascript spell the same debug type, the same setting
    name, and the same activation event — the drift that produces an extension
    which loads and never activates
- a breakpoint may be set in every language the debugger claims

**not checked, and not claimed.** nobody has installed this in vs code and
clicked through it. the contribution points it uses — `debuggers`,
`breakpoints`, `configuration`, `onDebugResolve:` activation, and
`registerDebugAdapterDescriptorFactory` — are vs code's documented ones and are
used the way the documentation describes them, but "documented correctly" and
"verified working" are different claims and only the first one is being made
here. so the MVP criterion this belongs to is **not ticked**

two specific things to check first, by someone who can:

1. that the extension activates and the session starts at all — the whole point
1. that the error from a missing `bpd` reaches the user. it is both shown with
    `showErrorMessage` and thrown, because how vs code renders an error thrown
    out of `createDebugAdapterDescriptor` is not something this project has
    seen. if it renders it, the sentence appears twice and one of the two should
    go
