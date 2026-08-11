# django templates

a breakpoint in `index.html` should stop while that template is rendering, on
that line, with a stack that shows the template — not the twenty frames of
`django/template/base.py` underneath it — and with the template context
available to inspect

this page used to be a design document written without django in front of it.
everything below is now **measured**, against django 6.1 on cpython 3.14, and
the places where the design was wrong are named as such rather than quietly
corrected

## why this is not a source map problem

django does not compile a template to python. `Template.compile_nodelist`
produces a tree of `Node` objects, and rendering walks that tree calling
`render_annotated` on each node. there is no generated line for a template line
to map to, so the [source map](source-mapping.md) machinery does not apply.
template frames have to be **synthesised**

that has a consequence for the crate layout, and it is the answer to a question
the architecture doc left open. `bpd_django` was drawn beside `bpd_sourcemap` as
a "locations in, locations out" crate, and **there is no such crate**, because
there is no location translation to do. what django integration actually needs
is python object access on the event path — reading a node's token, walking a
`Context` — and that can only live inside the agent. so the vocabulary is in
`bpd_core` with the rest of the domain, the machinery is
`crates/bpd_agent/src/templates.rs`, and nothing sits between them

## where the events come from

`NodeList.render` calls `node.render_annotated(context)` once per node.
`Node.render_annotated` is one code object, so PEP 669 can put `PY_START` on
**that one code object** — locally, via `set_local_events`. measured:
`set_local_events` accepts `PY_START`, and inside the callback the frame that is
running is the `render_annotated` frame with `self` and `context` already bound

when the callback fires, `self` is the node about to render, and that node
carries where it came from:

- `node.token.lineno` — the line in the template
- `node.origin.name` — the template's absolute path on disk

**`TextNode` overrides `render_annotated`**, and that is the first of the two
things about this hook that change what a user sees. a node of literal html
renders through `TextNode.render_annotated`, which is a different code object
and is not watched, so a line that is nothing but text produces no event. it is
therefore not a line a breakpoint can bind to, and one asked for there **moves**
to the next line that has a node which does render — the same rule, and the same
reported movement, as a breakpoint on a blank python line

the rule bpd applies is the general one rather than a `TextNode` special case: a
node is bindable only when `type(node).render_annotated is
Node.render_annotated`. any other subclass that overrides it is invisible to the
hook for the same reason and is excluded by the same test

the second thing is `{% extends %}`, and it is larger

## `{% extends %}` makes most of a template unrenderable

this is the bug this feature was found to have, and it is the exact wrongness
the project exists to prevent: a breakpoint reported **bound** in a place
nothing will ever stop

a template that opens with `{% extends %}` renders almost none of its own tree.
`ExtendsNode.render` renders the **parent's** nodelist. the parent's
`BlockNode.render` then reaches back through the block context for the
overriding block and renders `block.nodelist` — the child's block *body*,
directly. so:

- the child's `{% block %}` node is **never** passed to `render_annotated`. the
    node that renders is the *parent's* block node
- anything the child holds under its `{% extends %}` and outside every block is
    **never rendered at all**. it does not reach the output, whatever the
    context holds
- a `{% block %}` nested *inside* a block body renders normally, because by then
    django is walking a nodelist it really is rendering

measured, for a child holding `{{ stray }}` outside every block, an `{% if %}`
outside every block, and blocks at both depths — of the nine lines django parsed
into nodes, four are lines it will ever call `render_annotated` on

so the walk that registers a template carries a region: the root nodelist is
rendered, descending into an `ExtendsNode` is not, and descending into a
`BlockNode` from a region that is not rendered is rendered again. a node is
recorded as bindable only in a rendered region. the same walk with that rule
removed reports breakpoints bound on the `{% block %}` tag line, on `{{ stray }}`
and on a line after `{% endblock %}`, and the program runs to completion without
stopping at any of them — which is what
`crates/bpd_engine/tests/django.rs` pins

## the design doc was wrong about debug mode

the previous version of this page said `node.token` and `node.origin` are only
populated when the template engine is in debug mode, and that `bpd` must refuse
template debugging by name when it is off. **that is not true.**

`Parser.extend_nodelist` sets both, unconditionally:

```python
# Set origin and token here since we can't modify the node __init__()
# method.
node.token = token
node.origin = self.origin
```

there is no `engine.debug` anywhere near it. what debug mode actually chooses is
the **lexer**: `compile_nodelist` uses `DebugLexer` instead of `Lexer` when it is
on, and the only difference between the two is that `DebugLexer` records each
token's character `position`. both track `lineno` identically, in the same loop:

```python
result.append(self.create_token(token_string, position, lineno, in_tag))
lineno += token_string.count("\n")
```

measured end to end: the same template rendered with `'debug': True` and with
`'debug': False` produces the same node classes, the same `token.lineno` and the
same `origin.name`. only `token.position` differs — `(61, 75)` against `None` —
and `position` is what `Template.get_exception_info` uses to draw the yellow
error page. nothing bpd needs reads it

so **there is no debug-mode refusal, because there is nothing to refuse**.
writing one would be `bpd` declining to do something it can do correctly, which
is a different failure from the one the rule about failing loudly exists to
prevent, and just as much of a lie about the program. what stops that claim from
rotting is that the same test runs the whole fixture twice, once with the option
on and once with it off, and requires the binding, the stop and the frame chain
to be identical

the refusals that are real are the ones below

## what is actually refused, and why

| situation                                                                                             | what bpd says                                                                                                                            |
| ----------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------- |
| the file is on disk, the interpreter has compiled no python from it, and django is not in the process | `NotLoaded`, unchanged from the python case                                                                                              |
| the same, and django's template machinery **is** loaded                                               | `NotLoaded`, saying django has not parsed a template from that file yet and that it binds the first time one is loaded                   |
| django has parsed the template and no node renders at or after the line                               | `NoRenderedNode`, naming the last line that does render one, and naming both reasons a line has none — literal text, and `{% extends %}` |

the second is the template analogue of "a breakpoint in a module that has not
been imported is unbound and binds later", and it works the same way: it stays
in the set, and it is reported again, unprompted, the moment django parses the
template. what makes that possible is the second hook

## the second hook: seeing a template at all

binding needs the parsed node tree, and asking django for it would mean calling
`engine.get_template(name)` — which **parses the template**, populates the
loader cache, and is therefore the debugger changing the program. bpd does not
do that. it only ever observes what django loads on its own

`django.template.base.Template.__init__` compiles the nodelist as its last act,
so a local `PY_RETURN` on that one code object hands over a fully built template
with `self.origin` and `self.nodelist` set — measured. from there the tree is
walked through `node.child_nodelists`, which is how a `{% block %}` body and an
`{% if %}` branch are reached

that is why the **first** render of a template stops rather than being missed:
`Template.__init__` returns before `Template.render` is ever called, so the
breakpoint is bound before the first node is rendered. the same thing is what
makes a breakpoint in a template reached only through `{% include %}` work at
all — django parses it in the middle of the enclosing render, and the breakpoint
binds and fires within the same render

### the two hooks are not armed together

the design said both are armed only while a template breakpoint exists, and for
`Template.__init__` that is circular: a template breakpoint cannot **bind**
until django has parsed the template, and the only way bpd learns that a
template was parsed is this hook. waiting for a binding before arming it is
waiting for something that can never happen, and the first version of this
feature did exactly that and never bound anything

so they are armed on different conditions:

- `Template.__init__` — while any breakpoint in the set is one a template parse
    could answer. that is once per template django loads, for the whole process
- `Node.render_annotated` — only while a breakpoint really is bound in a
    template. this is the expensive one, once per node rendered, and it is off
    the rest of the time

both are resolved from `sys.modules` rather than by matching a path — django's
location on disk is not something bpd should be guessing at

### a module in `sys.modules` is not a module that has finished importing

cpython puts a module object in `sys.modules` when its import **begins**. so a
lookup landing on `django.template.base` while its body is still running finds a
module with none of its names on it, and one on
`django.template.loader_tags` finds the same

that is not a corner case here, it is the common path: importing a django module
registers a file, registering a file is what makes the agent resolve breakpoints
again, and django imports `loader_tags` from inside `Engine.__init__`. reading
attributes off either without expecting this crashes the debuggee with an
`AttributeError` about a partially initialized module

so `django.template.base` is read tolerantly — a module missing the names is
"not yet", and the next file django loads asks again — and
`django.template.loader_tags` is read at the one moment it is certainly
finished, which is when a `Template` has just been built. a `Template` implies
an `Engine`, and an `Engine` implies its builtin tag libraries were imported

## what it costs

deciding whether a node is interesting needs the node, and the node is only
reachable through the frame. so unlike every other event path in `bpd`, this one
**does** materialise a frame per event — there is no way not to, because
`PY_START` is handed a code object and an offset and the node is neither

what keeps it bounded:

- it is paid only while a template breakpoint is bound, and only on the one code
    object that hook is on. nothing else in the program is instrumented
- the first thing read is `self.token.lineno`, an integer, and it is compared
    against the set of lines any template breakpoint is bound to. a node whose
    line is in no template's breakpoint set costs that comparison and nothing
    more
- the template's file identity is only reached when the line already matched,
    and it is resolved from `origin.name` through a map built when the template
    was registered, so no `stat` happens on the event path

this is written down rather than argued: it is the one place the "a frame is
materialised only once a stop is decided" rule in
[architecture](architecture.md#the-callback-does-not-get-a-frame) does not hold,
and pretending otherwise would be a claim about the design that the code does not
keep

## the synthesised stack

**template frames are in the stack, interleaved with the python frames.** that
is a decision, and the alternative was a request of their own

a separate request would mean a client has to know to ask — and worse, a client
that did not ask would be shown twenty frames of `django/template/base.py` and
no template at all, which is the exact thing this page opens by saying must not
happen. so they are in `Stack::frames`, where every consumer sees them

what stops that from being a lie is `bpd_core::FrameKind`. a `Frame` carries a
`file`, a `line`, and a `kind`, and everything that is true of only one kind
lives inside it:

- `FrameKind::Python { function, first_line }` — `co_qualname` and
    `co_firstlineno`, a frame the interpreter really has
- `FrameKind::Template { node, python }` — the django node class, and the id of
    the `Node.render_annotated` frame underneath. a client that renders this as a
    python frame has to have gone out of its way to

the chain comes out of the python stack rather than being assembled from
guesswork: every `Node.render_annotated` frame in the walk is one template
frame, synthesised immediately above it. so `{% include %}` and `{% extends %}`
produce a nested chain because they really are nested calls. measured, at a
breakpoint on the variable in `part.html`, which `index.html` includes from
inside a `{% with %}` in a block, extending `base.html`:

```text
part.html:2   VariableNode
index.html:6  IncludeNode
index.html:4  WithNode
base.html:2   BlockNode
index.html:1  ExtendsNode
```

that fourth line is worth reading twice, and it is not a bug. `{% extends %}`
renders the **parent's** nodelist, and `base.html`'s `BlockNode.render` is what
looks up the overriding block from `index.html` and renders it. so the node that
is running is `base.html`'s, and the content under it is `index.html`'s. that is
the call chain django really has — and it is the same fact that makes the
child's own `{% block %}` line unbindable

## the template context

a template frame's variables are not python scopes, and asking for one is
refused with `Refusal::NotAPythonFrame`, which names the python frame that does
answer. what a template frame has is `Request::TemplateContext`

`django.template.Context` is a **stack of dicts**, and it is reported as one.
measured, at a variable inside a `{% with %}` inside a `{% block %}`:

```text
[['False', 'None', 'True'],
 ['footer', 'greeting', 'salute', 'shout', 'stray', 'trailing', 'who'],
 ['block'],
 ['greeting']]
```

four layers: django's builtins, the dictionary the render was given, the one
`BlockNode.render` pushes to hold `block`, and the one `{% with %}` pushed.
`greeting` is in two of them, and which one wins is what the template renders —
django's `Context.__getitem__` walks the layers from the last backwards and
takes the first that holds the name. merging them would be a report in which
that had already happened invisibly, so `TemplateContext::shadowed` names every
such name and the layers holding it, and `TemplateContext::resolve` does django's
walk in one place rather than leaving two front ends to choose a direction each

## evaluating in a template frame

an expression evaluated in a template frame is **template syntax**, not python.
the frame decides the language, which is why this needs no request of its own:
`Request::Evaluate` against a `FrameKind::Template` resolves by django's rules,
and against the `FrameKind::Python` frame underneath it — whose id the template
frame carries — it is python

the difference is not cosmetic, and it is what someone is usually debugging:

| expression          | python               | django                                                  |
| ------------------- | -------------------- | ------------------------------------------------------- |
| `user.profile.name` | attribute, attribute | dict key, then attribute, then list index, at each step |
| `items.0`           | `SyntaxError`        | the first element                                       |
| `called`            | the function object  | **the result of calling it**                            |
| `value\|upper`      | `TypeError`          | the filter applied                                      |

filters resolve because the expression is compiled with django's own
`FilterExpression` against a `Parser` built from the engine's registered
libraries and builtins, which is reachable from `context.template.engine`.
a lookup that fails is answered with django's own `VariableDoesNotExist` rather
than with `None`, for the reason a python expression that raises is answered
with the exception — and specifically rather than with the engine's
`string_if_invalid`, which is `''` by default and would be a debugger reporting
an empty string for a variable that is not there

## `runserver` reloads into a child, and the agent is not in it

**this is the largest practical limitation of the feature**, and it is not about
templates at all

`django.utils.autoreload.restart_with_reloader` calls `subprocess.run(args)` and
then does nothing but wait on the exit code. the process `bpd launch` attached to
is the supervisor; the **child** it spawned serves every request and renders
every template, and the agent is not in it. so under a plain

```sh
bpd launch manage.py runserver
```

no template breakpoint can fire, because the process holding them never renders
anything. nothing is misreported — the supervisor never imports the template
engine, so the hook never arms and the breakpoint is reported **unbound**, which
is true — but the answer looks like a broken feature until you know why

the "until you know why" part is smaller than it was: `bpd` now **says** when
the program it is debugging has started a python child, so a `runserver` session
reports the reloader's child rather than leaving an unbound breakpoint to be
interpreted. see [child processes](subprocesses.md)

it still does not follow the child, so the way to use this is to take the
reloader out:

```sh
bpd launch manage.py runserver --noreload
```

subprocess debugging is `M7a` in [the roadmap](../../ROADMAP.md)

## what is not covered

- **a `{% block %}` no parent template defines.** django adds every block of an
    extending template to the block context and renders the ones an ancestor
    asks for; a block whose name no ancestor has is simply never rendered. bpd
    reports a breakpoint in its body **bound**, and it will not fire. knowing
    otherwise would mean resolving `{% extends "..." %}` to a file through
    django's loaders, which parses the parent template — the debugger changing
    the program, which this feature refuses to do anywhere else. it is the
    template analogue of a breakpoint in a function nothing calls
- **jinja2 templates.** the same idea applies to a different node model and it
    is separate work rather than a variation of this one
- **writing a template context variable.** `Request::SetVariable` against a
    template frame is refused with the reason. the value would have to go into a
    chosen layer of `Context.dicts`, and which layer a write means is a question
    with no obvious right answer — it is not built rather than guessed at
- **the source around a template frame.** `bpd` shows source only when it can
    prove the file on disk is still the code that is running, and for a template
    there is no code object to prove it against. it is refused rather than shown
    unverified
- **stepping in template terms.** a step from a template frame is a step in the
    python underneath it. stepping node to node is a different operation and is
    not built
- **template partials** (`PartialTemplate`, django 6.1). they do not go through
    `Template.__init__`, so nothing registers them, and nothing claims to
- **template compilation errors.** those happen before anything is running, and
    are a linter's job

## how it is tested

`crates/bpd_engine/tests/django.rs`, against a real interpreter running real
django, with the program writing a marker before the render and another after it
— so a stop is proved by the second marker **not** having been written, the same
way every other stop in this project is proved

django is installed by `uv` into a tree beside the built agent and put on the
debuggee's own `sys.path` by the fixture, so running under `bpd` and running
directly see the same import state. the version is pinned in
`bpd_test::django::VERSION` and **asserted inside the debuggee**, the way the
cpython characterisation tests name their interpreter. django's template
internals are not a stable API, and a test that silently started measuring
something else would be worse than one that fails
