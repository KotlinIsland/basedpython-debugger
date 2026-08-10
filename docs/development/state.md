# the stopped state

when a thread is held, everything a person or an agent comes to believe about
the program is read here. so this page is mostly about the difference between a
thing `bpd` knows and a thing it can only assume, and about saying which is
which

what exists today is frames, the stack, scopes, values, object graph expansion
and expression evaluation. how a thread gets from one stop to the next —
stepping, pausing and the exception breakpoints — is
[stepping](stepping.md). none of this has a command line surface yet: it reaches
a user through the adapters, which are not built

everything on this page can be asked for one piece at a time, which is what a
tree walking front end does, or **all of it in one call** — a set of expressions,
the scopes of a chosen frame, the source around its line, under one byte budget.
that is [the state query](queries.md), and it is composed of the requests on this
page rather than being a second way of reading a value

## a frame identity says which stop it belongs to

a frame id is `{stop, depth}`: the number of the stop it was minted at, and how
far down the stack it is, with the frame that stopped at zero

DAP hands out an opaque integer that is valid until the next resume and looks
exactly the same afterwards, which is [the stale handle
problem](agent-interface.md#what-is-wrong-with-dap-for-an-agent) an agent hits
the moment it reasons across turns. carrying the stop in the id means a stale
one is **detected**, not guessed at:

```text
frame 2 of stop 1 belongs to a stop that has ended — the program is in stop 3
now. a frame id is valid for one stop, because the frame it named has run on
since. ask for the stack again
```

the frames themselves are walked the first time a stop is asked about one, and
dropped when it ends. a frame is materialised **only** at a stop, and never to
decide whether to stop — that is the rule the whole event path depends on, and
it is stated in [architecture](architecture.md#the-callback-does-not-get-a-frame)

## no frame of bpd's is ever in a stack

the agent is native, so the interpreter pushes no python frame to call a
callback and none of them can appear anywhere. `bpd` has exactly **one** python
frame in the whole process: the `-c` bootstrap the interpreter was entered
through, which is the outermost frame and the parent of the program's own module
frame

it is remembered when the agent arms itself, before the program has a frame of
its own, and the walk stops at it. so the stack at the entry stop is one frame
long — the program's module — and a walk that did not stop there would report a
frame in `<string>` running code the user never wrote

there is a second place a debugger's frames could leak into a stack and it does
not exist here: a breakpoint reached while a condition is being evaluated does
not fire at all, because cpython refuses to re-enter a tool's callback on a
thread that is already inside one. what remains is the stop a failed condition
leaves behind — the program held inside a `LINE` callback whose expression has
already unwound — and there the stack is the program's while the frames the
expression ran in are a separate thing that the stop carries as a traceback.
`a_stack_holds_no_frame_of_bpds_even_where_an_expression_of_bpds_just_ran`
asserts both halves

## the scopes are four things, not one mapping

`f_locals` merges a frame's own locals, the cells a nested function captures from
it, and the free variables it captures from an enclosing frame. that mapping is
the right thing to **evaluate** against, because it is what `LOAD_NAME` sees. it
is the wrong thing to **report**, because a closure and a global of the same name
are indistinguishable in it — and "a variable read from the wrong scope" is named
in the contract as something this project will not ship

so the names come from the code object, one scope at a time, and the values come
from the frame:

| scope | names | what it is |
| --- | --- | --- |
| `local` | `co_varnames` | the frame's own locals |
| `cell` | `co_cellvars` | locals of this frame that a nested function captures |
| `free` | `co_freevars` | variables captured from an enclosing frame |
| `global` | the mapping itself | `f_globals` |

two consequences that look like bugs and are not:

- an **argument a closure captures** is in `local` *and* in `cell`, because
    cpython puts it in both name lists. it is one variable that is both things
- a **module frame's locals are its globals**. cpython makes `f_locals` and
    `f_globals` the same object for one, so the two scopes answer identically

a module or a class body has no `CO_OPTIMIZED` flag and keeps its locals in an
ordinary namespace mapping rather than in slots, so its local scope is read as a
whole mapping. a class body whose metaclass prepared its own mapping — what
`enum` does — is not read at all, because reading it would mean calling that
mapping's own code

### unbound, and unreadable

a name can be in a scope and hold nothing, and that is not the same as absent:

- **unbound** — a local before its first assignment. reporting it as missing
    would hide a variable the frame has, and reporting it as `None` would be a
    value the program does not hold
- **unreadable** — a class body's free variables. the code object names them and
    the value lives in a cell that only the function object holds, so no frame
    exposes it. they are not unbound; they hold something `bpd` cannot see, and
    an expression cannot reach them either — `eval` resolves a name through the
    frame's namespaces, and the cell is in neither, so the answer is the
    interpreter's own `NameError`

## writing a variable, and the write that goes nowhere

PEP 667 is a large part of why the minimum is 3.13: `f_locals` is a write-through
proxy rather than a snapshot, so a debugger can honestly set a local and the
compiled code will load what was written

it is honest **only** with one check. the proxy also accepts a write of a name
the code object does not have, keeps it, and reads it back — while the function
goes on reading the fast locals the compiler gave it. measured in a bare
interpreter by
`the_interpreter_accepts_a_write_of_a_name_the_program_can_never_read`:

```pycon
>>> frame.f_locals['invented'] = 5
>>> frame.f_locals['invented']
5
>>> invented
NameError: name 'invented' is not defined
```

so a write is refused unless the name is already in the scope it was asked for,
and the refusal says which scopes do hold it. the same applies to a free variable
of a class body: the write would land in the class namespace and the compiled
code would never look there, so it is refused rather than performed somewhere
harmless

the value is a python expression, evaluated in the frame. the answer is the value
read back **out of the frame** afterwards, rather than the value that was handed
over, because what the frame holds now is the thing that was asked about

`a_local_written_at_a_stop_is_the_value_the_program_goes_on_to_use` is the
acceptance, and it does not assert that `bpd` reads the value back — it resumes
the program and asserts on the number the program itself computed and wrote to
disk

## representing a value: storage, not behaviour

reading a value does not run the program. that is the rule, and it decides every
detail below

structural forms are read through cpython's **concrete** interface — the object's
own storage — and never through the abstract protocol. a `list` subclass that
overrides `__getitem__`, `__iter__` and `__len__` is read from the list's
storage, which is what the object really holds, and the type name reported
alongside it says what it really is. `a_value_is_read_from_its_storage_and_not_
from_the_types_own_code` puts exactly that class in a frame, with every one of
those methods raising

| kind | how it is read | reported as |
| --- | --- | --- |
| `int` | `int.__repr__`, the unbound slot | decimal text |
| `float` | `float.__repr__` | python's own text |
| `str` | the string's utf-8 | the characters, not a repr |
| `bytes`, `bytearray` | the buffer, copied | lowercase hex |
| `list`, `tuple` | `PyList_GetSlice` / the tuple's items | a sequence |
| `set`, `frozenset` | `list()`, and only for an **exact** one | a sequence |
| `dict` | `PyDict_Items` | pairs, not names |
| anything else | its instance dictionary | an object |

a few of those are decisions rather than mechanics:

- **`int` and `float` are text.** a python `int` has no width, and a json number
    that silently became a float would be a different value. `float` carries
    python's own repr so `inf`, `nan` and `-0.0` survive — json can express none
    of the three, and `null` for a float is a debugger reporting a value the
    program does not have. the unbound slots, not `str()`, so a subclass cannot
    change the number that is reported about what it holds
- **a mapping is pairs.** a key can be any object. `name: value` would be a lie
    about every dict that is not keyed by strings
- **a set has to be an exact one.** cpython has no concrete accessor for set
    storage — there is no `PySet_GetItem` — so the only way in is iteration, and
    a subclass can make iteration its own code. `list`, `tuple` and `dict` have
    concrete accessors, so a subclass of one is read like the thing it is
- **a string that is not utf-8** — lone surrogates, which is what
    `surrogateescape` produces for an undecodable filename — has them replaced,
    and the answer says so rather than replacing them quietly

### the two ways program code can run, and both are in the request

- `attributes`, **on by default**, reads an object's `__dict__`. this is
    storage: for an ordinary object it is a slot read that reaches no
    `__getattr__`, no property and no descriptor. a type is free to make
    `__dict__` its own code, and then this runs it — which is why it can be
    turned off for a program full of proxies or mocks. a type that keeps no
    instance dictionary at all, a `__slots__` class or one implemented in C, is
    reported as having none rather than as being empty
- `repr`, **off by default**, calls `__repr__`. that is arbitrary user code: it
    can hang, mutate the program, or reach the network, and the result is
    labelled as having come from `__repr__` rather than being presented as the
    value

the roadmap for this work asked for `__repr__` "under an explicit budget and
timeout". the budget exists; **the timeout does not, and is not claimed**.
interrupting user python needs a second thread that can raise into the first,
which is more than releasing the GIL buys: `PyThreadState_SetAsyncExc` lands at
the next bytecode boundary, and a `__repr__` blocked in a C call never reaches
one. a timeout that could not fire would be a promise the debugger cannot keep,
so what is shipped instead is the opt-in and a sentence saying that a `__repr__`
which hangs hangs the thread it was asked on

## expansion, and saying what was left out

a request carries five bounds, and **every bound that bites is named in the
answer, in the place where it bit**:

| bound | default | what it limits |
| --- | --- | --- |
| `depth` | 3 | levels of container or object opened |
| `children` | 100 | children read from one container |
| `text` | 1024 | characters of one string, bytes of one `bytes` |
| `budget` | 8192 | bytes for the whole answer |
| `attributes` / `repr` | on / off | what may run |

these defaults are a starting point rather than a settled answer. the byte budget
is spending an agent's context window, and [what it is worth is an open
question](agent-interface.md#still-open) that cannot be closed until there is an
agent surface to measure it against

`budget` means one more thing inside [a state query](queries.md#the-budget-is-one-budget),
where it bounds the **whole** answer rather than each read in it. that is the
same number doing the same job at a larger scope, and the parts it could not
reach are named for the reason a cut value is

the budget counts the text a value carries, its type name, and a fixed cost per
value for the envelope around it. an answer can exceed it by the envelope of the
one value that discovered it was gone, and by nothing else

two rules keep an expansion from being confidently incomplete:

- **an integer is whole or absent.** every other kind of text is cut and says how
    much there was. half of a number is a different number, so an `int` too long
    for the limit is left out entirely, with its length
- **a cycle terminates and names where it came round to.** a structure that
    points back at itself stops at the repeat, reporting the path where that
    object is already open. stopping silently would look exactly like a structure
    that ended

### a scope is read at the depth that fits

a set of variables is read at the deepest whole level the budget allows, rather
than at the level asked for until it runs out. the difference is not cosmetic:
every module namespace begins with `__builtins__`, so the second way spends the
whole answer on one variable nobody asked about and reports the rest as missing —
a true statement about the wrong thing

when the level is reduced the answer says so, with both numbers:

```text
the request asked for a depth of 3 and the byte budget fitted 0, so every value
here was read to 0. ask again with a larger `budget`, or for one value rather
than a whole scope
```

## evaluating an expression

an expression is compiled at the request and evaluated against the chosen frame's
own namespaces — `f_globals` and `f_locals`, the merged mapping, because that is
what `LOAD_NAME` sees and resolving a name any other way is how a debugger reads
a variable from the wrong scope. it is the same machinery breakpoint conditions
use, and this thread's breakpoints are held off while it runs for the same reason

**an expression that fails returns the exception.** one that raises, and one that
does not compile, are both answers rather than refusals: the interpreter is the
authority on what an expression is, and reporting `None` for either would be the
debugger inventing a value the program never produced. the traceback names
`<bpd evaluation>`, so nothing mistakes the debugger's expression for the
program's own code

a refusal is a different thing, and there are only four: a frame id from a stop
that has ended, a depth the stack does not reach, a name that is not in the scope
it was asked for, and a name in a scope the frame does not expose. each names the
cause and what to do instead

## every read says what was moving while it was taken

a stop holds one thread and the rest of the program keeps running, on every
build — see [threads](threads.md). so a read taken here is a **sample** of a
program that is still moving, and every answer carries the mode it was taken in
rather than leaving that to be assumed:

| mode | what it says |
| --- | --- |
| `non_stop` | one thread was held and the rest of the program kept running |
| `stop_the_world` | everything that could be held was, and here is what could not |

one thing is a snapshot in either mode: **the held thread's own stack**. it is
inside a monitoring callback and cannot return, so nothing can pop a frame of it
while it is held. what the mode qualifies is everything the frames point *at* —
a global another thread rewrites, a list another thread appends to

there is still no request here that walks a thread `bpd` is **not** holding. its
frames are moving, and a stack read off one would be a description of a moment
that had already gone; where a running thread is, stated as the sample it is,
belongs to the thread census — see [threads](threads.md)

the same caveat applies inside one value. a container is read with cpython's own
copy — `PyList_GetSlice`, `PyDict_Items` — so what comes back is internally
consistent rather than half of one state and half of another. it is what the
object held while it was read, and in non-stop mode nothing stops another thread
from changing it immediately afterwards. what does stop them, for as long as it
lasts, is
[stopping the world](threads.md#stopping-the-world) — and even that names the
threads it could not stop

## how it is tested

`crates/bpd_engine/tests/state.rs`, against a real interpreter, with the fixture
programs keeping a marker after the breakpoint line so no test takes the agent's
word for where the program is

two of them establish ground truth in a **bare** interpreter, with no agent
anywhere near it, because they are statements about cpython rather than about
`bpd`: that `f_locals` accepts and reads back a write the compiled code can never
see, and that a module frame's locals and globals are the same object
