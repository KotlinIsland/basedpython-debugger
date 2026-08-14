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

## the two commands, and what neither of them does

```sh
bpd-release assemble --binary target/release/bpd \
  --agent 3.13=.../libbpd_agent.so \
  --agent 3.14=.../libbpd_agent.so \
  --out dist/bpd
bpd-release verify dist/bpd
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

## it is driven on every push

the `agents` CI job builds an agent for 3.13 and 3.14, assembles a layout out of
them, verifies it, and **launches a program through it on both interpreters**.
that last step is what makes the rest of it worth having: a layout that
assembles and verifies and cannot launch is precisely the failure above, and
only running it finds one
