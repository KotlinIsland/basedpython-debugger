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
agents/3.15/libbpd_agent.so
agents/3.15t/libbpd_agent.so
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

**nothing in this crate publishes.** there is no upload, no signature and no
network call anywhere in it or in the `agents` CI job that drives it on every
push. it reads files that already exist, writes a directory, and reads that
directory back

what publishes is [the release workflow](#publishing), which is a different
thing in a different file — and it does it by running these same three commands
on five machines and then uploading what they produced

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
basedpython_debugger-0.1.0-py3-none-macosx_11_0_arm64.whl
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
| per interpreter (`cp313-…`) | 25                       | 25                   |
| per platform (`py3-none-…`) | **5**                    | **5**                |

five agents rather than six, because `3.13t` cannot be built at all — pyo3
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
basedpython_debugger-0.1.0.data/scripts/bpd           ->  <venv>/bin/bpd
basedpython_debugger-0.1.0.data/data/agents/3.13/…    ->  <venv>/agents/3.13/…
basedpython_debugger-0.1.0.data/data/agents/3.14/…    ->  <venv>/agents/3.14/…
basedpython_debugger-0.1.0.data/data/agents/3.14t/…   ->  <venv>/agents/3.14t/…
```

on windows that first line is `…data/scripts/bpd.exe -> <venv>/Scripts/bpd.exe`,
and the `.exe` is not decoration. windows runs a file by its extension, so a
layout that carried the binary as `bpd` would install a `Scripts/bpd` that pip
is perfectly happy with and windows cannot execute — the same shape of failure
as an agent under a name nothing looks for, and invisible in the same way. so
the name comes from `bpd_release::binary_name`, which is the platform's answer
rather than a constant, and `the_binary_is_carried_under_the_name_the_platform_runs_it_by`
is the test

`Root-Is-Purelib: false` is the field with teeth. true would have pip put the
payload in `site-packages`, and nothing looks for an agent there

### the name in the filename is not the name in the metadata

pip reads the distribution and the version back **out of the filename**, whose
five fields are joined with `-`. so a `-` inside any one of them is not a
cosmetic problem: it makes a filename that parses cleanly as a *different*
distribution at a different version, installs, and is then the wrong thing

the packaging specs answer that by escaping the name — every run of `-`, `_` or
`.` becomes a single `_` — and carrying the real one in `METADATA`, which is the
field pypi reads to decide which project an upload belongs to. `bpd-release`
does both, and refuses anything it cannot do it for:

| field        | as the project spells it | in the filename        |
| ------------ | ------------------------ | ---------------------- |
| distribution | `basedpython-debugger`   | `basedpython_debugger` |
| version      | `0.0.1a1`                | `0.0.1a1`              |

the version has no second spelling, and that is the point of checking it. the
**cargo** version of this workspace is `0.0.1-a1`, because semver puts a dash
before a prerelease and pep 440 does not. handing that to `--version` would
write `…-0.0.1-a1-py3-none-…`, which pip reads as version `0.0.1` with a build
tag — a wheel that installs as a version nobody released. so it is refused, and
`the_python_version_is_the_crates_version` holds `pyproject.toml` to the crates
so the two spellings can never be two numbers

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

## publishing

a **tag is the whole trigger**, and the tag is the version: pushing `v0.0.1a1`
runs `.github/workflows/release.yaml`, which builds five wheels, installs every
one of them, debugs five interpreters through each, and — after a person
approves it — uploads them to pypi. there is no input to the workflow, because
an input is a second place the version could come from

### what a release is

five wheels, each carrying five agents, and no sdist:

| platform tag             | built on           | how                                         |
| ------------------------ | ------------------ | ------------------------------------------- |
| `manylinux_2_17_x86_64`  | `ubuntu-latest`    | inside `quay.io/pypa/manylinux2014_x86_64`  |
| `manylinux_2_17_aarch64` | `ubuntu-24.04-arm` | inside `quay.io/pypa/manylinux2014_aarch64` |
| `macosx_11_0_arm64`      | `macos-latest`     | natively                                    |
| `macosx_10_12_x86_64`    | `macos-15-intel`   | natively                                    |
| `win_amd64`              | `windows-latest`   | natively                                    |

the linux builds happen **inside pypa's image** rather than on the runner, and
that is the whole reason the tag can say `2_17`. what glibc a binary needs is a
fact about the toolchain that compiled it, so the only honest way to claim 2.17
is to compile against 2.17 — building on `ubuntu-latest` and writing `2_17` on
the result is a wheel pip installs on a machine that cannot run it. the image is
also where every interpreter comes from: `/opt/python/cp313-cp313`, `cp314-cp314`,
`cp314-cp314t`, `cp315-cp315`, `cp315-cp315t`

there is no windows-arm64 wheel, and its absence is deliberate: nothing in this
repository has ever built or tested that target, and a wheel for a platform
nobody has run the suite on is exactly the "probably right" this project does
not ship

### one script, five machines

every one of those five runs `scripts/build_release.sh`, which is a script
rather than steps in a workflow because the linux builds happen in a container
and the others do not — two copies of it would be two copies that drift. it
builds an agent per interpreter, builds `bpd`, assembles, verifies, writes the
wheel, and then does the part that matters:

```sh
scripts/build_release.sh 0.0.1a1 macosx_11_0_arm64 \
  3.13=/…/python3.13 3.14=/…/python3.14 3.14t=/…/python3.14t \
  3.15=/…/python3.15 3.15t=/…/python3.15t
```

it makes a venv with the **oldest** interpreter carried, installs the wheel it
just wrote with a real `pip`, and launches a program through the installed `bpd`
on **every** interpreter — so a wheel that reaches pypi is one that has debugged
five pythons from an install rather than from a checkout. it also asks each
interpreter what it is before building an agent for it, because an agent filed
under a tag it was not compiled against refuses at import on somebody else's
machine, and that is the only moment both halves are in one place

### what stops a wrong release

| checked                                    | why                                                                                    |
| ------------------------------------------ | -------------------------------------------------------------------------------------- |
| the tag is `pyproject.toml`'s version      | and `pyproject.toml` is the crates' version, so one comparison pins all three          |
| `ci` concluded `success` on this commit    | a tag can be pushed at any commit, including one whose suite never ran                 |
| every interpreter says what its tag claims | an agent under the wrong tag imports and reads the wrong offsets                       |
| the layout verifies before it is a wheel   | the last moment anything can check the digests                                         |
| the installed wheel debugs every tag       | a layout that assembles, verifies and cannot launch is the failure this all exists for |
| there are five wheels, not four            | a platform silently missing from a release gets no wheel and pip falls back to nothing |
| a person approves the `pypi` environment   | a version on pypi can be yanked and never replaced                                     |

### there is no api token

the upload uses **trusted publishing**: github mints a short-lived token for the
one workflow, and pypi verifies it against a publisher configured on the project.
nothing in this repository holds a credential, so there is nothing here to leak
or to rotate

it is configured once, on pypi, under the project's *publishing* settings — or
as a **pending publisher** before the first release, since the project does not
exist there until something is uploaded:

| field        | value                  |
| ------------ | ---------------------- |
| pypi project | `basedpython-debugger` |
| owner        | `KotlinIsland`         |
| repository   | `basedpython-debugger` |
| workflow     | `release.yaml`         |
| environment  | `pypi`                 |

the environment is named on both sides on purpose: pypi will not accept a token
that did not come from it, and github will not run the `publish` job until
somebody approves it — **provided the environment has a required reviewer**. an
environment with no protection rule on it does not pause for anything; it only
labels the job. so it is created once, in the repository's settings, under
*environments -> pypi -> required reviewers*, and without that the approval step
in the table above is not a step at all

### releasing

1. `pyproject.toml` and the workspace `Cargo.toml` carry the version — the two
    spellings of it, which `the_python_version_is_the_crates_version` holds
    together
1. push that, and let `ci` finish. the release refuses a commit whose suite did
    not pass
1. `git tag v0.0.1a1 && git push origin v0.0.1a1`
1. the five wheels build, install, and debug five interpreters each
1. approve the `pypi` environment, and it uploads
