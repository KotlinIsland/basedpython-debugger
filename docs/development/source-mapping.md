# source mapping

a design document

the interpreter only ever knows about python. a user who set a breakpoint in
`app.by` or in `index.html` expects to stop in `app.by` or in `index.html`, and
expects the stack to be reported the same way. the layer that makes those two
views agree is the source map, and it is the single easiest place in a debugger
to be quietly wrong

## the rule

**a map either resolves a location or it errors.** there is no identity
fallback

a fallback that returns the raw line when the map has no entry produces a stack
frame that points at the wrong line of the wrong file, and looks exactly like a
correct one. that is the worst failure mode a debugger has. so a missing entry
is an error that names the file and the line, and the user finds out that their
build is stale instead of finding out that their program is haunted

## the three cases

### python

identity. a `.py` file maps to itself. this case exists in the type system so
that everything downstream handles one shape, not two

### basedpython

`.by` source transpiles to `.py`, and the interpreter runs the `.py`. a
breakpoint in `.by` has to become a line in the generated python, and a frame in
the generated python has to become a line in `.by`

the map has to be **produced by the transpiler**, not reconstructed afterwards.
a lowering can move a statement, split one statement into several, or generate
python that corresponds to no source line at all — a `match` arm becomes a
chain, a null-coalescing expression duplicates its left hand side. no
after-the-fact heuristic recovers that reliably, and a heuristic that is right
most of the time is the fallback this document just banned

this is a dependency on the basedpython transpiler emitting a map, and it is
tracked as such in `ROADMAP.md`. until it does, `bpd` debugs the generated
python and says that is what it is doing

two properties the map must have, and which are worth testing directly:

- **bidirectional and total on what it covers.** every generated line that can
    be executed maps back to a `.by` line, and mapping forwards then backwards
    lands somewhere sensible. a generated line with no origin — a lowering's own
    scaffolding — is marked as such rather than attributed to whichever source
    line was nearest
- **verified against the artefact it maps.** the map records a hash of the
    source and of the generated python. a stale map is detected at load, and is
    an error, not a warning. this is the check that catches "i edited the file
    and forgot to rebuild", which is otherwise indistinguishable from a debugger
    bug

### django templates

different in kind, and covered in [django templates](django-templates.md).
django does not compile templates to python code, so there is no generated line
to map to. that case needs synthesised frames rather than a line map

## what a frame looks like after mapping

a mapped frame carries both locations: the one the user asked about and the one
the interpreter is actually at. the second is not shown by default, and is one
request away

this matters for the same reason the whole document does. when a user does not
believe the debugger, they need to be able to see what it saw
