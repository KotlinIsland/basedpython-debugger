# what is holding an object

"why is this still alive". the collector already knows — it has to, to decide
what to free — and this is the way to ask it

```json
{
  "command": "bpd/retainers",
  "arguments": { "frameId": 1, "expression": "session" }
}
```

the object is named by an **expression in a frame**, the way `evaluate` names
one. there is no other way to point at an object: it has no id of its own that
outlives being asked about

## the answer says where inside each holder it sits

"a dict holds it" is almost nothing. "the value under `'session'`" is the answer
somebody can act on, so `through` carries it wherever the holder's shape can be
read:

| holder             | what `through` says         |
| ------------------ | --------------------------- |
| a dict             | the value under a named key |
| a dict, as a key   | that it is a key of it      |
| a list or tuple    | which index                 |
| a set or frozenset | that it is an element of it |
| an object          | which attribute             |
| anything else      | nothing — see below         |

**a set is not given an index.** its iteration order is its hash table's and
moves when the table is resized, so the position it happens to be reached at is
not a place the object is — reported as a sequence's index it says the program
holds it somewhere it does not, and it is not stable enough to be true twice

**absent is not "nowhere".** it is a holder whose shape could not be read — a C
type with its own traversal reaches its referents by a route no python-level
inspection has — and saying nothing is the difference between a debugger that
does not know and one that invents

nothing here runs the program's own code. a retainer is described by its type and
how much it holds rather than by `repr()`, because a repr calls the object's own
`__repr__`, and `dir()` is avoided for the same reason: both are the program's
code run to answer a question *about* the program

the rule is wider than those two, and the quiet ones are what break it. `len()`
is `__len__`, so a count is asked only of the builtin containers, matched by
**exact** type — a dict subclass can override it. and a dict's entries are read
straight off the table rather than looked up key by key, because a lookup hashes
what it is given, which is `__hash__` and `__eq__`

what that costs if it is got wrong is not only tidiness. the program's code run
during a walk can mutate the heap the walk is describing, which is the one
property this page measured and kept — and a key whose `__hash__` raises would
fail a retainer query that had nothing to do with it

## `coverage` is on every answer and is not a footnote

a list of holders reads as "these are the holders". this walk is the collector's
referent graph, and it is blind to two whole categories:

- **objects the collector does not track.** an `int`, a `str`, a `float` — a
    container holding one is found, the thing itself never is
- **holders that are not python objects.** a reference held by C or rust is a
    refcount rather than something the collector walks. **bpd's own are among
    them**: the agent holds handles to code objects and to recorded task stacks,
    so this answer cannot see the debugger asking it

so `coverage` is carried every time rather than when something looks wrong — an
agent cannot tell "nothing was hidden" from "this front end does not say"

## three things this was expected to need, and did not

all measured before any of it was built:

- **it does not need `unsafe`.** the assumption was a native walk over the
    referent graph, which means `tp_traverse`, which `unsafe_code = "deny"`
    forbids workspace-wide with no opt-outs. `gc.get_referrers` is C-implemented
    and reaches the same graph from safe code
- **it is not slow.** 0.7 ms on a 55,000-object heap, 2.5 ms on 205,000, 9.8 ms
    on 805,000 — linear, and an interactive answer at every size
- **it does not perturb the heap.** the object count was identical before and
    after. the version that keeps the index in python does not have that
    property: it grew a 55,000-object heap to 167,000, which is the "perturbs
    what it measures" problem in numbers

## and one it does need, which is the opposite of what was expected

the concern was that bpd's own frames would appear as retainers and have to be
filtered out. measured on 3.13, 3.14 and 3.15: **a frame does not appear as a
retainer of its own local**, even materialised through `sys._getframe` with its
`f_locals` read — which is the state every frame bpd holds is in. PEP 667 made
`f_locals` a snapshot rather than a live dict

`every_holder_is_found_and_says_where_inside_itself_the_object_sits` asserts that
absence, so a release that changes it is noticed here rather than in a report
nobody can explain

the real hole is the inverse, and it is the second bullet of `coverage`: bpd's
rust-side references are invisible to the walk rather than noise in it

## a template frame is refused

django resolves template syntax to a **new** value — `{{ user.name }}` is a
lookup that builds a string — so a walk would find what the resolution just made.
that is a true answer to a question nobody asked, so it is refused by name, with
the python frame underneath that does answer
