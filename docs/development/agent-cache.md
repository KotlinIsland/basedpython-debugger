# the agent cache

every launch stages the agent into a per-user cache named after the sha-256 of
the artifact's bytes — [launching](launching.md) has the design and the reason.
this page is the other half of it: the cache is never pruned, so it only grows,
and `bpd cache` is how a person sees it and takes it back

```sh
bpd cache
bpd cache clear
bpd cache clear --keep-current
```

## the number this exists for

one agent build is about 5.6 MB, and a rebuild has different bytes, so it takes
a new entry and leaves its predecessor exactly where it was. that is not
overhead a rule can amortise — it is one copy per build, for ever

on the machine this was written on, after two days of building the agent:

```text
cache        /Users/morgan/.cache/bpd/agents
entries      89
size         447.4 MiB (469206896 bytes)
current      35ec95e90058b885e39956a1cb007e5fafe9fd667fc282be129ffdd281eb729b
             not staged yet — the next launch will put it there
```

the design note that introduced the cache said a pruner was "more failure
surface than a megabyte is worth". the megabyte was 448 of them. that is the
whole of what changed the decision, and the two other reasons in that note
survived it intact — which is why what got built is a command rather than a
pruner

## nothing here happens on its own

there is no pruning on launch, no age limit, no eviction, no cleanup thread.
neither question a pruner would have to answer can be answered from inside one
`bpd`:

- **what is still needed.** an entry is on the `PYTHONPATH` of every debuggee
    launched from it, including debuggees of another `bpd` on the same machine
    that this one cannot see. a cache is per user, not per process
- **what can even be removed.** windows refuses to delete a shared object a
    process has loaded, which is exactly what an entry in use looks like. a
    background pruner would fail there routinely, for a reason the user never
    asked about

a person asking is the only thing that knows. so the cache is read when it is
asked about and changed when it is told to be, and the two cost nothing when
nobody asks

## the report

`bpd cache` names the directory, counts the entries, adds up what they hold, and
says which entries the agents **this** `bpd` carries would stage into — the ones
whose removal costs the next launch a cold load, and the only ones there is a
reason to keep

there is one per interpreter tag, because a `bpd` carries one agent per tag and
each is what a launch on the interpreter it is for would stage —
[launching](launching.md#which-agent-is-staged) has the layout. a checkout
carries a single untagged artifact instead, and it is reported as what it is:

```text
current      35ec95e90058b885e39956a1cb007e5fafe9fd667fc282be129ffdd281eb729b
             the agent for python 3.13 — not staged yet — the next launch will put it there
current      6be4c6f43b6e1b9a2a86a8c0c8c1f2f0c1a2b3c4d5e6f708192a3b4c5d6e7f80
             the agent for python 3.14 — staged, 5.3 MiB (5620704 bytes) — clearing it costs the next launch a cold load of the agent
```

a cache that is not there is said plainly and is not a failure. it is what a
machine that has never launched a debuggee looks like, and asking about it does
not create it:

```text
cache        /Users/morgan/.cache/bpd/agents
             it is not there — nothing has been staged yet, and it holds nothing
current      35ec95e90058b885e39956a1cb007e5fafe9fd667fc282be129ffdd281eb729b
             not staged yet — the next launch will put it there
```

sizes are printed in the unit a person reads and in the bytes they really are,
so the number can be checked against `du` rather than taken on trust

## clearing

`bpd cache clear` removes every entry. `--keep-current` leaves the ones holding
the agents this `bpd` carries — one per interpreter tag — so the next launch
does not pay a cold load of one, about 120 ms on macOS, measured in
[what bpd costs](overhead.md)

it is worth having for exactly that reason and no other: the entries a person is
about to use again are the pieces of the cache with a cost attached to deleting
them, and the report already points at them. `--keep-current` names each agent
it could not keep because the cache did not hold it, rather than leaving a
silence that reads like it kept something

there is no flag that clears anything the refusals below stop. a `--force` would
be a way to turn the checks off, and the checks are the reason it is safe to run

## what it refuses, and what it never does

deleting is the one thing on this page that cannot be undone, so the rules are
about what is **not** removed:

| the cache                                     | what happens                               |
| --------------------------------------------- | ------------------------------------------ |
| is not there                                  | reported, and not created                  |
| is a link, or is not a directory              | refused, by staging's own check            |
| is somebody else's, or others can write to it | refused, the same way                      |
| holds something staging never wrote           | reported, and nothing is removed           |
| holds an entry that will not go               | named with the reason, and a non-zero exit |

the trust rule is not a second implementation. it is
`bpd_engine::agent::trusted`, the same function a launch is refused by, called
over metadata the reader takes itself — so a directory `bpd` will not stage into
is a directory `bpd cache` will not touch either. reading is the one place it
differs from staging: staging creates the directory before it checks it, and a
report that made the thing it was asked to describe would be answering a
question nobody asked

**an entry is a 64 character hex directory holding the agent and nothing else.**
that is all staging writes, so it is all that gets removed. a file in the cache
that is not one, a directory that is not named like a digest, a link — even one
named exactly like an entry — or a file inside an entry that is not the staged
module: each is reported with what it is, and the clear stops without removing
anything. a directory with a surprise in it may not be the directory `bpd`
thinks it is

the one thing `bpd` writes into the cache that is **not** an entry is the
temporary file a publish renames into place, named `.staging…`. it is gone by
the time a launch returns, and one that outlived its `bpd` is a crash mid-write
— so it is reported like any other surprise rather than swept up. removing it
by hand is safe once no `bpd` is launching; deciding that from in here would
mean deciding what another process is in the middle of, which is the thing this
page starts by saying cannot be done

removal is the known names and then `remove_dir`, never a recursive delete. what
was inside was read and accounted for when the cache was opened, and `remove_dir`
refuses a directory that has gained something since instead of carrying it off —
so a `bpd` publishing into that very entry at that very moment ends as a named
failure rather than as a deletion nobody described

## an entry that will not go

this is the one a debugger has to get right, because the alternative is a
comfortable lie:

```text
removed      0 entries
reclaimed    0 bytes

failed       ~/.cache/bpd/agents/35ec…729b/bpd_agent.so
             Permission denied (os error 13)
```

and the process exits non-zero. removing four of five entries and printing
"cleared" would leave a person believing they had reclaimed something they had
not, which is the class of wrongness this project exists to not produce. every
entry that could not be removed is named with what the operating system said
about it, the rest are still attempted — one entry loaded by a debuggee on
windows should not stand between a person and the other eighty-eight — and the
exit code says the answer is not the whole answer

`bpd cache` exits non-zero for the same reason when the directory holds
something it cannot account for, or when the current entries cannot be named
because this `bpd` carries no agent at all. the report is still printed:
what it says is true, it is just not all of what was asked

## what is not covered by a test

the failure `bpd cache clear` is really written for — windows refusing to delete
a shared object a debuggee has loaded — cannot be produced on the machines this
is developed on. what is tested instead is the same shape from the other
direction: an entry directory nothing can be removed from, on unix, in
`crates/bpd_engine/src/cache.rs` and `crates/bpd/tests/cache.rs`. that pins the
part a script depends on — the entry is named, the other entries still go, and
the command does not report success — against a removal the operating system
really refuses, rather than against a code path nobody reached
