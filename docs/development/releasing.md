# the layout a release is shipped as

`bpd` carries **an agent per interpreter tag** and chooses one at launch by what
the interpreter says it is. that is the one thing an installed `bpd` does that no
development build does: a checkout holds a single agent, built against whichever
interpreter `PYO3_PYTHON` last named

so the layout only exists at release time, and `bpd-release` is what builds it.
[the agent cache](caches.md) is where the chosen agent then goes; this page is
about what is chosen from

## what it is

```text
bpd
MANIFEST
agents/3.13/libbpd_agent.so
agents/3.14/libbpd_agent.so
agents/3.14t/libbpd_agent.so
```

`bpd_engine::agent` scans `agents/` beside the binary and beside its parent,
reads each directory name as an interpreter tag, and joins **exactly**
`cargo_artifact_name()` onto it. a directory whose name is not a tag is skipped
rather than guessed at, because reading one loosely hands a release an agent
built for another interpreter — which imports and then reads the wrong offsets

## the three commands, and what none of them does

```sh
bpd-release assemble --binary target/release/bpd \
  --agent 3.13=.../libbpd_agent.so \
  --agent 3.14=.../libbpd_agent.so \
  --out dist/bpd
bpd-release verify dist/bpd
bpd-release wheel --layout dist/bpd \
  --version 0.1.0 --platform macosx_11_0_arm64 --out dist/
```

**nothing publishes.** there is no upload, no tag, no signature and no network
call anywhere in this crate or in the CI job that drives it. it reads files that
already exist, writes a directory, and reads that directory back. what happens
to it afterwards is a decision nobody has made — and the roadmap says so rather
than leaving a half-armed release workflow in the tree

it is a separate binary from `bpd` on purpose. a `bpd package` subcommand would
appear in `bpd --help` for every person who ever installs one, describing
something only this repository's release ever does

## the name is not the one it was built under

this is the mistake the crate exists to prevent, and it was made while writing
it. an agent copied into the layout under the name it happened to have —
`agent-3.13.so`, or whatever a build script called it — produces a layout that

- assembles with no complaint
- **verifies** against its own manifest, because the manifest describes what was
    written
- and answers `bpd carries no agent build at all` at every launch

measured, before the fix. so `assemble` takes the file name from
`bpd_engine::agent::cargo_artifact_name()` — the same function the scan joins —
rather than from the input. the unit test that missed it wrote the paths out as
text, which is why it agreed with the bug

## what it refuses

a packaging tool that does its best with bad input produces a directory somebody
ships. every one of these produces nothing at all instead, and the checks all
run **before** the first write, so a refusal never leaves a half-built layout
behind:

| what                                 | why it is not a warning                                                                                                 |
| ------------------------------------ | ----------------------------------------------------------------------------------------------------------------------- |
| no binary at the path given          | a release is assembled from artifacts that exist; this builds nothing                                                   |
| no agents at all                     | it would debug nothing: every launch refuses with the tags carried, and that list is empty                              |
| a tag that is not a tag              | parsed by `InterpreterTag::parse`, the same parser the scan uses — a spelling this rejects is one no launch would match |
| the same tag twice                   | which one shipped would be whichever was copied last, so the release depends on argument order                          |
| an agent file that is not there      | named, with the `PYO3_PYTHON=…` line that builds it                                                                     |
| an output directory holding anything | a layout assembled over an older one carries an agent for a tag this build never made                                   |

## the manifest is evidence, not description

`MANIFEST` is one line per file — `sha256:…`, two spaces, the path — which is
the shape `sha256sum` writes, so a person without `bpd` can check a release with
a tool they already have. it is sorted, so assembling the same inputs twice
produces the same manifest byte for byte; a release nobody can rebuild is one
whose contents are an assertion

`verify` reads it back and compares every digest. that is the same discipline
`bpd_core::SourceMap` holds and for the same reason: a digest that is written and
never checked says nothing at all. a file that changed is named with both
digests, and a file the manifest names and the layout does not hold is named too

## the wheel is the same layout, delivered by pip

`pip install` is how python developers find python tooling, and it is where an
editor looks: the vs code extension resolves `bpd` off `PATH`, which a venv puts
it on. so a layout can also be written out as a wheel

### it is tagged for a **platform**, not for an interpreter

```text
basedpythondebugger-0.1.0-py3-none-macosx_11_0_arm64.whl
```

the `bpd` binary is not a python extension. it links nothing of cpython —
`otool -L` names no libpython — and it drives interpreters it is handed rather
than the one it lives inside. only the **agent** is version specific, and the
agent is loaded by the *debuggee*, which is very often not the interpreter
anybody ran `pip install` in

so tagging the wheel `cp313-cp313-…` would tie it to the wrong thing. it would
ship one copy of the same binary per python version, and leave each install able
to debug exactly the interpreter it was installed into — a tool made smaller by
its packaging:

|                             | wheels, over 5 platforms | copies of the binary |
| --------------------------- | ------------------------ | -------------------- |
| per interpreter (`cp313-…`) | 15                       | 15                   |
| per platform (`py3-none-…`) | **5**                    | **5**                |

three agents rather than four, because `3.13t` cannot be built at all — pyo3
refuses the free-threaded build of any cpython below 3.14, which
[python support](python-support.md) records

this is the shape `ruff` and `uv` ship in, and for the same reason: a native
binary that pip is only the delivery mechanism for

### the wheel layout **is** the install layout

no engine code knows a wheel exists, and that is not a coincidence.
`bpd_engine::agent` looks for `agents/<tag>/` in the directory holding the
running binary **and the one above it** — which for an installed binary is
`<prefix>/bin` and `<prefix>`. a wheel's `.data` directory installs into the
environment's own scheme directories, where `scripts` is the first of those and
`data` is the second:

```text
basedpythondebugger-0.1.0.data/scripts/bpd            ->  <venv>/bin/bpd
basedpythondebugger-0.1.0.data/data/agents/3.13/…     ->  <venv>/agents/3.13/…
basedpythondebugger-0.1.0.data/data/agents/3.14/…     ->  <venv>/agents/3.14/…
basedpythondebugger-0.1.0.data/data/agents/3.14t/…    ->  <venv>/agents/3.14t/…
```

`Root-Is-Purelib: false` is the field with teeth. true would have pip put the
payload in `site-packages`, and nothing looks for an agent there

### the platform tag is taken, not detected

what manylinux level a linux binary satisfies is a fact about the **toolchain
that built it**, which this program cannot see. guessing produces a wheel pip
installs happily on a machine whose libc is too old, and the failure lands at
the first launch rather than at install time. so it is an argument, and a tag
with a `-` in it is refused by name — a wheel filename joins its fields with
dashes, so one inside a field makes a name pip reads as a different distribution
entirely

### there is no sdist

and there will not be one. building this from source needs cargo **and one
interpreter per agent**. an sdist pip could not actually build is a package that
installs by appearing to and then fails at the first launch, which is the same
class of lie as a layout that verifies and cannot run. what ships is what was
built and verified

## it is driven on every push

the `agents` CI job builds an agent for 3.13 and 3.14, assembles a layout out of
them, verifies it, and **launches a program through it on both interpreters**.
that last step is what makes the rest of it worth having: a layout that
assembles and verifies and cannot launch is precisely the failure above, and
only running it finds one

the same job then writes that layout out as a wheel, installs it into a **3.14**
venv with a real `pip`, and debugs **3.13** through it. the interpreter mismatch
is the assertion: it is what a per-interpreter wheel would fail, and no
assertion about the contents of a zip can stand in for it
