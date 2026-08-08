# django templates

a design document

a breakpoint in `index.html` should stop while that template is rendering, on
that line, with a stack that shows the template — not the twenty frames of
`django/template/base.py` underneath it — and with the template context
available to inspect

## why this is not a source map problem

django does not compile a template to python. `Template.compile_nodelist`
produces a tree of `Node` objects, and rendering walks that tree calling
`render` on each node. there is no generated line for a template line to map
to, so the [source map](source-mapping.md) machinery does not apply. template
frames have to be **synthesised**

## where the events come from

`NodeList.render` calls `Node.render_annotated` once per node. that is a normal
python method, so it has a code object, and PEP 669 can put a `PY_START` event
on **that one code object** — locally, via `set_local_events`, and only while a
template breakpoint exists

when the callback fires, `self` is the node about to render, and that node
carries where it came from:

- `node.token.lineno` — the line in the template
- `node.origin` — which template file, including through `{% include %}` and
    `{% extends %}`

so one local event on one code object yields a precise template location per
render step, with no patching of django, no import hook, and nothing at all
when no template breakpoint is set

this is the same idea as the rest of the debugger: instrument the smallest
possible number of code objects, and let `DISABLE` handle everything else

## the requirement, and the loud failure

`node.token` and `node.origin` are only populated when the template engine is
in **debug mode** — `'OPTIONS': {'debug': True}` on the `DjangoTemplates`
backend, which `DEBUG = True` turns on by default

without it there is no line information at all. `bpd` does not guess, does not
fall back to the python frames, and does not silently skip template
breakpoints. it reports that template debugging is unavailable on this engine,
names the setting, and continues debugging the python normally

## the synthesised stack

a template frame is presented as a frame:

- its source is the template file, its line is the node's line
- its scope is the template `Context`, which is a stack of dicts —
    presented as the layered thing it is, not flattened, because two layers
    shadowing each other is exactly what someone is usually debugging
- nested templates produce nested frames. `{% include %}` and `{% extends %}`
    are visible as the call chain they are

the python frames that implement the render are still there, one request away.
a user debugging django itself needs them, and a user debugging their template
does not

## evaluating in a template frame

an expression evaluated in a template frame is **template syntax**, not python.
`user.profile.name` resolves through django's variable lookup rules — dictionary
key, then attribute, then list index, with callables invoked — which is not what
the same text means in python, and the difference is often the bug

filters resolve too, so `value|date:"Y-m-d"` is answerable

evaluating python in a template frame is available explicitly, against the
underlying python frame, and it is labelled as such

## what is not covered

- jinja2 templates. the same approach applies to a different node model, and it
    is a separate piece of work rather than a variation of this one
- template compilation errors. those are a linter's job, and they happen before
    anything is running
