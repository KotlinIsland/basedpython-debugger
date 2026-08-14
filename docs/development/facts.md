# facts, and how long one lasts

`variables` answers what a scope holds. that is a statement about a **moment**,
and it is the only thing a debugger has traditionally been asked for

a client that draws what the *next* branch will do needs something else. it is
not reading the program's state, it is reading the program's **source**, and
what it wants from the debugger is the seed: what is true here, and how far past
here can it be carried

a **fact** is that pair. `bpd/facts` over DAP and the `facts` tool over MCP

```json
{
    "frameId": 3,
    "names": ["limit", "items", "self.mode"],
    "limit": { "text": 1024, "depth": 4 }
}
```

```json
{
    "proved": [
        {
            "name": "limit",
            "scope": "local",
            "observed": { "observed": "is_int", "text": "5" },
            "stability": { "stability": "permanent" }
        },
        {
            "name": "items",
            "scope": "local",
            "observed": { "observed": "has_length", "length": 3 },
            "stability": { "stability": "until", "mutation": "contents" }
        }
    ],
    "silent": [
        {
            "name": "self.mode",
            "why": {
                "silence": "would_run",
                "member": "mode",
                "owner": { "module": "myapp", "qualname": "Runner" }
            }
        }
    ],
    "mode": "non-stop"
}
```

three things in that answer are the whole design, and each of them is a thing a
value read cannot say

## the stability is the point, and only the debugger can know it

`limit` holds `5` and `items` holds a list of three. both readings are true. only
one of them can be carried over a line that has not run yet, and **nothing in the
source says which** — it depends on the object

so the judgement is made here, from cpython itself, and all three inputs are slot
reads:

| what is read | what it decides |
| --- | --- |
| `Py_TPFLAGS_HEAPTYPE` | a heap type is what a `class` statement makes, and `__class__` can be assigned on one. cpython refuses it for a static type, so `type(x) is int` is permanent and `type(x) is User` is not |
| `tp_dictoffset` | non-zero means instances keep a dictionary, so any attribute of one can be assigned |
| the type object itself | against the table of builtins whose storage *is* their value. a `tuple`'s length cannot change and a `list`'s can |

a fact is therefore `permanent` or `until` something named. what `until` names is
a thing the client can look for in the source it is reading — an assignment, an
attribute write, a `__class__` swap — which is why it is a cause rather than a
confidence score

**what stability does not cover is the name.** a name is rebound by code, and
code is what the client is reading. a debugger that guessed at it would be
answering a question it cannot see the input to, so it does not: `permanent`
means *short of rebinding the name*, and the client owns that half

## one name is several facts, because they go stale differently

an empty `list` is three things at once: it is exactly a `list`, it has a length
of zero, and it is falsy. the first cannot change — `list` is a static type. the
second and third can, with the next `append`

so a fact is one observation rather than one name, and each carries its own
stability. rolling them into a per-name verdict would take the weakest of the
three and throw away the two that were solid

## it runs none of the program

the rule [`values`](state.md) holds is that reading a value does not run the
program. here it is stricter, because a fact is carried *forward*: a truthiness
taken by calling `__bool__` would be a claim about code that has not run, built
on one call to code that did

so there is no `repr` escape hatch on this request and there is not going to be
one. everything comes out of an object's storage or its type's slots:

- **the class** through `PyType_GetQualName` and `PyType_GetModuleName`, not
    through `getattr` — which would reach a metaclass `__getattribute__`
- **the length** only when `type(value)` is **exactly** one of the builtins,
    through cpython's concrete interface. a `list` subclass may well not override
    `__len__`, but deciding that means judging whether what the MRO holds is
    cpython's own slot, and a wrong answer there is the debugger calling into the
    program while claiming it did not. so a subclass gets its class facts and no
    length: an absent fact is not a claim, and the answer is shorter rather than
    wrong
- **enum membership** through `PyType_IsSubtype` and the member's own `_name_`.
    the `isinstance` builtin would ask `EnumMeta.__instancecheck__`, which is
    python, and `.name` is a `DynamicClassAttribute`, which is a descriptor call.
    a program that never imported `enum` is never asked
- **a dotted path** one instance dictionary at a time. a segment the type has a
    **data descriptor** for is refused rather than read, because a data descriptor
    wins over the instance dictionary and reading it means calling its `__get__`.
    that is what `self.mode` hit in the example above

## a name that proves nothing says why

every name asked about comes back in exactly one of `proved` and `silent`. a
name missing from both would be indistinguishable from one bound to something
uninteresting

| why | what it means |
| --- | --- |
| `unbound` | no scope the frame can see has it. a local before its first assignment reads the same way, because from here it is the same situation |
| `missing` | a segment of a path is not in the object's own storage. reaching it would mean `__getattr__` |
| `would_run` | the type puts a data descriptor in the way, named with the class that defines it |
| `too_deep` | the path has more segments than the request's `depth`. a refusal rather than a truncation: answering about the fourth segment of a five segment path is a fact about a different thing |

## the bounds

`text` is how much of a value one fact may carry. a value longer than it produces
**no fact** rather than a shortened one — a fact is compared against source, and
`x == "abc…"` is a different claim from `x == "abcd"` with no way to mark it as
approximate. this is the same rule the integer digits follow in
[the stopped state](state.md): half of a number is a different number

`depth` is how many segments of a dotted path to follow, because a client
composing paths from source can compose arbitrarily long ones

a misspelled bound is refused rather than silently taken as the default, for the
reason `detail` is: it is an instruction the client gave and bpd would otherwise
ignore

## the mode qualifies the reading, not the judgement

whether a `list`'s length **can** change is a property of `list`, and it is true
in either thread mode

whether it **was** three when it was read is a sample like any other. on a
non-stop read another thread can have appended to it since, which is why the
answer carries the mode it was taken in — the same rule everything else here
follows, in [threads](threads.md)

## what this is for

the client this was built for is a data flow analysis in an editor: seed a type
checker's narrowing with what is true at the line the program is stopped on, and
draw what the branches below it will do. intellij idea has had this for java
since 2020, where `final` fields and `@NotNull` do the work `stability` does here

python has no `final` and no purity annotations, so the judgement has to come
from the object, at runtime, which is exactly what a debugger is holding. that is
why this is a capability of `bpd` rather than something the analysis works out —
and why the honest answer for a value it cannot judge is `silent` rather than a
guess
