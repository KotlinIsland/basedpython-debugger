//! django templates: the two code objects bpd watches, and what it reads there
//!
//! django does not compile a template to python. `Template.compile_nodelist`
//! builds a tree of `Node` objects and rendering walks it, so there is no code
//! object for a template line and no source map that could produce one. what
//! there is instead is one method that runs once per node —
//! `Node.render_annotated` — which is a normal python function with a normal
//! code object, and PEP 669 can watch that one code object locally
//!
//! ## the two hooks
//!
//! - `PY_START` on `Node.render_annotated` is how a template line is reached.
//!   the frame it starts has `self`, the node about to render, and `context`
//! - `PY_RETURN` on `Template.__init__` is how a template is *seen*. it
//!   compiles its nodelist as its last act, so the frame that is returning
//!   holds a fully built template with its `origin` and its `nodelist`
//!
//! the second is what makes the first bindable. asking django for a template
//! instead — `engine.get_template(name)` — would **parse** it and populate the
//! loader cache, which is the debugger changing the program. so bpd only ever
//! observes what django loads on its own, and a breakpoint in a template django
//! has not parsed is unbound with that as the reason until it does
//!
//! they are **not** armed on the same condition, and assuming they were is how
//! the first version of this bound nothing at all. `Template.__init__` is armed
//! while any breakpoint is one a template parse could answer — waiting for a
//! template breakpoint to bind before arming the only thing that can make one
//! bind is waiting forever. `Node.render_annotated` is the expensive one, once
//! per node rendered, and it is armed only while a breakpoint really is bound
//! in a template
//!
//! ## what django will never render
//!
//! a template that opens with `{% extends %}` renders almost none of its own
//! tree: `ExtendsNode.render` renders the *parent's* nodelist, and the parent's
//! `BlockNode.render` reaches back for the overriding block and renders its
//! **body** directly. so the child's `{% block %}` node is never passed to
//! `render_annotated`, and anything the child holds outside every block is
//! never rendered at all. binding to one of those lines would be reporting a
//! breakpoint that can never fire — see [`Region`]
//!
//! ## the one place a frame is materialised to decide
//!
//! everywhere else in this crate an event is answered from a code object's
//! address and a line number, and a frame is built only once a stop is decided.
//! here it cannot be: the node is the whole question and the node is only
//! reachable through the frame. what bounds it is that the hook is on one code
//! object, armed only while a template breakpoint is set, and that the first
//! thing read is an integer compared against the lines any template breakpoint
//! is bound to

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock};

use bpd_core::{Binding, SourceBreakpoint, Unbound};
use pyo3::prelude::*;

use crate::conditions::Plan;
use crate::events;
use crate::files::{self, FileId};

/// the module every hook is resolved from
///
/// by import name rather than by path: where django is on disk is not something
/// bpd should be guessing at, and `sys.modules` is the interpreter's own answer
const TEMPLATE_MODULE: &str = "django.template.base";

/// where `{% extends %}` and `{% block %}` live
///
/// a separate module from [`TEMPLATE_MODULE`], and it is read at a different
/// time — see [`Tags`]
const LOADER_TAGS_MODULE: &str = "django.template.loader_tags";

/// the code objects bpd watches, resolved once django is importable
#[derive(Debug)]
struct Hooks {
    /// `Node.render_annotated`, where a template line is reached
    render: Py<PyAny>,
    /// `Template.__init__`, where a template becomes visible
    init: Py<PyAny>,
    /// the `Node.render_annotated` function itself
    ///
    /// what makes "does this node's class override it" answerable. `TextNode`
    /// does, so a line of literal text produces no event and cannot hold a
    /// breakpoint — and binding has to know that rather than reporting a
    /// breakpoint that silently never fires
    inherited: Py<PyAny>,
    /// `django.template.base.Parser`, which a filter expression compiles against
    parser: Py<PyAny>,
    /// `django.template.base.FilterExpression`, django's own expression parser
    ///
    /// held rather than imported when it is wanted, for the reason every other
    /// handle the agent uses is: reaching the import system from inside a
    /// monitoring callback is what corrupted line numbers once already
    filter_expression: Py<PyAny>,
}

/// the two `django.template.loader_tags` classes a walk is decided against
///
/// held apart from [`Hooks`] because they cannot be read at the same time.
/// `sys.modules` holds a module object from the moment its import *begins*, so
/// a lookup that happens while `django.template.loader_tags` is executing finds
/// a module with none of its names on it yet — and django imports that module
/// while building an `Engine`, which is exactly when the agent is being asked
/// to resolve breakpoints
///
/// so these are read at the one moment the module is certainly finished: a
/// `Template` exists, therefore an `Engine` exists, therefore the engine's
/// builtin tag libraries — of which this is one — were imported
#[derive(Debug)]
struct Tags {
    /// `ExtendsNode`, which makes almost all of a template unrenderable
    extends: Py<PyAny>,
    /// `BlockNode`, the one door back into a template an `{% extends %}` skips
    block: Py<PyAny>,
}

/// one template django has parsed, as far as breakpoints are concerned
#[derive(Debug, Default)]
struct Parsed {
    /// line -> the node classes that render at it, in tree order
    ///
    /// only nodes that render through `Node.render_annotated`. a node whose
    /// class overrides it reaches no event, so a line that has only those is
    /// not a line a breakpoint can bind to
    lines: BTreeMap<u32, Vec<String>>,
}

#[derive(Debug, Default)]
struct State {
    hooks: Option<Hooks>,
    /// resolved the first time a template is walked, and not before
    tags: Option<Tags>,
    /// every template seen, by the identity of the file it came from
    parsed: BTreeMap<FileId, Parsed>,
    /// `origin.name` -> that file's identity, worked out when it was registered
    ///
    /// so the event path never calls `stat`: by the time a node's line has
    /// matched, the string its origin carries is already known
    by_name: BTreeMap<String, FileId>,
    /// what a bound template breakpoint does, by the template and line it is on
    armed: BTreeMap<(FileId, u32), Vec<Arc<Plan>>>,
    /// every line any template breakpoint is bound to, whatever the template
    ///
    /// the event path's first question, and the one that rejects almost every
    /// node for the cost of one integer comparison
    lines: BTreeSet<u32>,
    /// whether any breakpoint in the set could be answered by a template parse
    ///
    /// the two hooks are not armed together, and this is why. a template
    /// breakpoint cannot be *bound* until django has parsed the template, and
    /// the only way bpd learns that is the `Template.__init__` hook — so waiting
    /// for a binding before arming it is waiting for something that can never
    /// happen. it is armed while any breakpoint is one a parse could answer,
    /// which is one call per template django loads
    ///
    /// the `Node.render_annotated` hook is the expensive one — once per node
    /// rendered — and it stays off until a breakpoint really is bound
    watching_parses: bool,
}

static STATE: RwLock<State> = RwLock::new(State {
    hooks: None,
    tags: None,
    parsed: BTreeMap::new(),
    by_name: BTreeMap::new(),
    armed: BTreeMap::new(),
    lines: BTreeSet::new(),
    watching_parses: false,
});

fn read() -> std::sync::RwLockReadGuard<'static, State> {
    STATE
        .read()
        .expect("the template lock is only held for map operations, which do not panic")
}

fn write() -> std::sync::RwLockWriteGuard<'static, State> {
    STATE
        .write()
        .expect("the template lock is only held for map operations, which do not panic")
}

/// find the two code objects, if django's template machinery is loaded
///
/// `sys.modules` rather than `import`: importing django from the agent would
/// put a package in the debuggee that the program never asked for, and doing it
/// from inside a monitoring callback is the trap that corrupted line numbers
/// once already. so this only ever finds a django the program imported itself
///
/// a module is in `sys.modules` from the moment its import **begins**, so a
/// lookup can land on one whose body has not run yet and which has none of its
/// names. `django.template.base` is imported while the agent is being asked to
/// resolve breakpoints — the import of one django module registers a file,
/// which is what makes the agent look again — so this is a state that really
/// happens rather than a defensive branch
///
/// it is not an error and it is not a missed template: nothing can have been
/// parsed by a module that has not finished importing. the answer is "not yet",
/// and the next file django loads asks again
///
/// returns whether the hooks are available now
pub(crate) fn resolve_hooks(python: Python<'_>) -> PyResult<bool> {
    if read().hooks.is_some() {
        return Ok(true);
    }

    let modules = PyModule::import(python, "sys")?.getattr("modules")?;
    let Ok(module) = modules.get_item(TEMPLATE_MODULE) else {
        return Ok(false);
    };
    let (Ok(node), Ok(template), Ok(parser), Ok(filter_expression)) = (
        module.getattr("Node"),
        module.getattr("Template"),
        module.getattr("Parser"),
        module.getattr("FilterExpression"),
    ) else {
        return Ok(false);
    };

    let inherited = node.getattr("render_annotated")?;
    let render = inherited.getattr("__code__")?;
    let init = template.getattr("__init__")?.getattr("__code__")?;

    write().hooks = Some(Hooks {
        render: render.unbind(),
        init: init.unbind(),
        inherited: inherited.unbind(),
        parser: parser.unbind(),
        filter_expression: filter_expression.unbind(),
    });
    Ok(true)
}

/// the loader-tag classes, read from a module that has finished importing
///
/// only ever called from [`registered`], where a `Template` has just been built
/// — see [`Tags`] for why the timing is the whole of it
fn resolve_tags(python: Python<'_>) -> PyResult<(Py<PyAny>, Py<PyAny>)> {
    if let Some(tags) = &read().tags {
        return Ok((tags.extends.clone_ref(python), tags.block.clone_ref(python)));
    }

    let modules = PyModule::import(python, "sys")?.getattr("modules")?;
    let Ok(loader_tags) = modules.get_item(LOADER_TAGS_MODULE) else {
        unreachable!(
            "a django `Template` was built, so an `Engine` imported its \
             builtin tag libraries, of which `django.template.loader_tags` is \
             one"
        );
    };
    let extends = loader_tags.getattr("ExtendsNode")?.unbind();
    let block = loader_tags.getattr("BlockNode")?.unbind();

    let pair = (extends.clone_ref(python), block.clone_ref(python));
    write().tags = Some(Tags { extends, block });
    Ok(pair)
}

/// whether django's template machinery is loaded in this process
///
/// what separates "that file is not python and nothing could have parsed it"
/// from "django is here and has not parsed it yet", which are different things
/// to tell a user about an unbound breakpoint
pub(crate) fn available() -> bool {
    read().hooks.is_some()
}

/// the code objects the hooks are on, for whoever arms them
pub(crate) fn hook_codes(python: Python<'_>) -> Vec<Py<PyAny>> {
    read().hooks.as_ref().map_or_else(Vec::new, |hooks| {
        vec![hooks.render.clone_ref(python), hooks.init.clone_ref(python)]
    })
}

/// what the template breakpoints want of one code object
///
/// half of what the interpreter is told about it, for the reason
/// [`crate::breakpoints::local`] is half: `set_local_events` replaces a code
/// object's whole mask
pub(crate) fn local(address: usize) -> events::Local {
    let state = read();
    let Some(hooks) = &state.hooks else {
        return events::Local::default();
    };

    events::Local {
        line: false,
        py_return: state.watching_parses && hooks.init.as_ptr() as usize == address,
        py_start: !state.armed.is_empty() && hooks.render.as_ptr() as usize == address,
    }
}

/// whether this code object is one the hooks are on
///
/// asked on the `PY_START` path before anything else, because the answer
/// decides whether the interpreter may be told to forget the code object
pub(crate) fn is_render_hook(address: usize) -> bool {
    read()
        .hooks
        .as_ref()
        .is_some_and(|hooks| hooks.render.as_ptr() as usize == address)
}

/// whether this code object is the one a template becomes visible through
pub(crate) fn is_init_hook(address: usize) -> bool {
    read()
        .hooks
        .as_ref()
        .is_some_and(|hooks| hooks.init.as_ptr() as usize == address)
}

/// register the template a returning `Template.__init__` has just built
///
/// returns the file identity when this is the **first** sighting of it, which
/// is the only moment a template breakpoint's answer can change from "django
/// has not parsed that" to a binding
pub(crate) fn registered(python: Python<'_>) -> PyResult<Option<FileId>> {
    let frame = events::current_frame(python)?;
    let Ok(template) = frame.getattr("f_locals")?.get_item("self") else {
        // the frame of `Template.__init__` has `self` bound before its body
        // runs, so this cannot happen for the code object the hook is on
        unreachable!("`Template.__init__` returned without `self` in its frame");
    };

    let origin = template.getattr("origin")?;
    let name = origin.getattr("name")?;
    let Ok(name) = name.extract::<String>() else {
        return Ok(None);
    };

    // a template compiled from a string carries `<unknown source>`, which is
    // not a file and never resolves. it is left out for the reason a code
    // object whose `co_filename` is not a file is: nothing could ever bind to
    // it, and retaining one per parse would grow without bound
    let Ok(identity) = files::identify(std::path::Path::new(&name)) else {
        return Ok(None);
    };
    if read().parsed.contains_key(&identity) {
        return Ok(None);
    }

    let inherited = {
        let state = read();
        let Some(hooks) = &state.hooks else {
            unreachable!("the init hook only fires while the hooks are resolved");
        };
        hooks.inherited.clone_ref(python)
    };
    let (extends, block) = resolve_tags(python)?;
    let classes = Classes {
        inherited: inherited.bind(python),
        extends: extends.bind(python),
        block: block.bind(python),
    };

    let mut parsed = Parsed::default();
    walk(
        &template.getattr("nodelist")?,
        &classes,
        &name,
        Region::Rendered,
        &mut parsed,
    )?;

    let mut state = write();
    state.by_name.insert(name, identity.clone());
    state.parsed.insert(identity.clone(), parsed);
    Ok(Some(identity))
}

/// the django classes a walk is decided against, already bound
struct Classes<'py, 'a> {
    /// `Node.render_annotated`, the one function the hook is on
    inherited: &'a Bound<'py, PyAny>,
    /// `ExtendsNode`
    extends: &'a Bound<'py, PyAny>,
    /// `BlockNode`
    block: &'a Bound<'py, PyAny>,
}

/// whether django will ever call `render_annotated` on the nodes of a nodelist
///
/// a template that opens with `{% extends %}` renders **almost none of its own
/// tree**. `ExtendsNode.render` renders the *parent's* nodelist, and the
/// parent's `BlockNode.render` reaches back for the overriding block and
/// renders `block.nodelist` — the child's block *body*, directly. so
/// everything the child holds under its `{% extends %}` is skipped except the
/// bodies of its blocks, and the `{% block %}` tags themselves are skipped too
///
/// measured against django 6.1: a child that extends and holds `{{ stray }}`
/// outside any block, an `{% if %}` outside any block, and a
/// `{% block %}` tag renders none of the three, while a `{% block %}` *nested
/// inside* a rendered block body renders normally
///
/// this is what separates a template breakpoint that binds from one that would
/// be reported bound and never fire
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Region {
    /// django renders these nodes
    Rendered,
    /// `{% extends %}` is above these nodes and django renders none of them
    Extended,
}

/// collect every node of a nodelist and everything nested under it
///
/// `child_nodelists` is django's own answer to "what is inside this node" — a
/// `{% block %}` body, both branches of an `{% if %}` — and walking it is what
/// makes a breakpoint inside a tag bind rather than only one at the top level
///
/// a node is only kept when its origin is the template being walked. an
/// `{% extends %}` node's children are the *including* template's, and binding
/// one of them to the wrong file is exactly the quiet wrongness this project
/// refuses
fn walk(
    nodelist: &Bound<'_, PyAny>,
    classes: &Classes<'_, '_>,
    file: &str,
    region: Region,
    parsed: &mut Parsed,
) -> PyResult<()> {
    for node in nodelist.try_iter()? {
        let node = node?;

        let origin = node.getattr("origin")?.getattr("name")?;
        let here = origin.extract::<String>().is_ok_and(|name| name == file);

        // `type(node).render_annotated is Node.render_annotated`. `TextNode`
        // overrides it, so a node of literal html renders through a code object
        // the hook is not on and reaches no event. reporting a breakpoint on
        // one as bound would be reporting a breakpoint that never fires
        let renders = node
            .get_type()
            .getattr("render_annotated")?
            .is(classes.inherited.as_any());

        if region == Region::Rendered
            && here
            && renders
            && let Ok(token) = node.getattr("token")
        {
            let line: Option<u32> = token.getattr("lineno")?.extract().ok();
            if let Some(line) = line.filter(|line| *line > 0) {
                let class: String = node.get_type().getattr("__name__")?.extract()?;
                parsed.lines.entry(line).or_default().push(class);
            }
        }

        // `{% extends %}` puts everything below it out of reach, and a
        // `{% block %}` down there is the one door back in: django hands its
        // body to the parent's block of the same name, which renders it
        let below = if node.is_instance(classes.extends)? {
            Region::Extended
        } else if region == Region::Extended && node.is_instance(classes.block)? {
            Region::Rendered
        } else {
            region
        };

        for name in node.getattr("child_nodelists")?.try_iter()? {
            let name = name?;
            if let Ok(child) = node.getattr(name.extract::<String>()?.as_str())
                && !child.is_none()
            {
                walk(&child, classes, file, below, parsed)?;
            }
        }
    }
    Ok(())
}

/// bind one breakpoint against a template django has parsed
///
/// the same two rules the python side has, applied to a node tree instead of a
/// line table: a line that has no node moves to the next line that does, and a
/// request past the last such line is unbound naming it
pub(crate) fn resolve(
    request: &SourceBreakpoint,
    identity: &FileId,
    plan: &Arc<Plan>,
    armed: &mut BTreeMap<(FileId, u32), Vec<Arc<Plan>>>,
) -> Binding {
    let state = read();
    let Some(parsed) = state.parsed.get(identity) else {
        return Binding::Unbound {
            reason: Unbound::NotLoaded {
                file: request.file.clone(),
                templates_available: state.hooks.is_some(),
            },
        };
    };

    let Some((&line, nodes)) = parsed.lines.range(request.line..).next() else {
        return Binding::Unbound {
            reason: Unbound::NoRenderedNode {
                file: request.file.clone(),
                requested: request.line,
                last_rendered: parsed.lines.keys().next_back().copied(),
            },
        };
    };

    let plans = armed.entry((identity.clone(), line)).or_default();
    plans.push(Arc::clone(plan));
    plans.sort_unstable_by_key(|plan| plan.id);

    Binding::BoundInTemplate {
        line,
        nodes: nodes.clone(),
        evaluation: plan.evaluation(),
    }
}

/// replace what the template hooks watch for
///
/// `watching_parses` is whether any breakpoint in the set is one a template
/// parse could answer — see [`State::watching_parses`] for why that is a
/// different question from whether one is bound
pub(crate) fn rearm(armed: BTreeMap<(FileId, u32), Vec<Arc<Plan>>>, watching_parses: bool) {
    let lines = armed.keys().map(|(_, line)| *line).collect();
    let mut state = write();
    state.armed = armed;
    state.lines = lines;
    state.watching_parses = watching_parses;
}

/// where a node about to render is, and what the breakpoints there do
#[derive(Debug)]
pub(crate) struct Hit {
    /// the template's path on disk, as `origin.name` carries it
    pub(crate) file: String,
    /// the line of it the node's token begins on
    pub(crate) line: u32,
    /// the breakpoints bound there, smallest id first
    pub(crate) plans: Vec<Arc<Plan>>,
}

/// what the breakpoints want of the node that is about to render, if anything
///
/// the order is the whole cost story. the node's line is an integer and it is
/// compared against the lines *any* template breakpoint is bound to, so a node
/// in a template nothing is watching costs one lookup. only once that matches
/// is the origin read and turned into a file identity, through a map built when
/// the template was registered — so nothing here touches the filesystem
pub(crate) fn rendering(python: Python<'_>) -> PyResult<Option<Hit>> {
    if read().lines.is_empty() {
        return Ok(None);
    }

    let frame = events::current_frame(python)?;
    let Ok(node) = frame.getattr("f_locals")?.get_item("self") else {
        unreachable!("`Node.render_annotated` started without `self` in its frame");
    };

    // a node built by a tag's own code and rendered outside a nodelist has no
    // token. it is not a node any breakpoint is bound to — binding only ever
    // sees nodes that came through `Parser.extend_nodelist` — so there is
    // nothing here to decide and nothing to guess
    let Ok(token) = node.getattr("token") else {
        return Ok(None);
    };
    let Ok(line) = token.getattr("lineno")?.extract::<u32>() else {
        return Ok(None);
    };
    if !read().lines.contains(&line) {
        return Ok(None);
    }

    let file: String = node.getattr("origin")?.getattr("name")?.extract()?;
    let state = read();
    let Some(identity) = state.by_name.get(&file) else {
        return Ok(None);
    };
    let Some(plans) = state.armed.get(&(identity.clone(), line)) else {
        return Ok(None);
    };

    Ok(Some(Hit {
        file,
        line,
        plans: plans.clone(),
    }))
}

/// whether this frame is a `Node.render_annotated` one
///
/// what the stack walk synthesises a template frame over. it is asked of every
/// frame of a stack that is being walked anyway, at a stop, so it is not on any
/// path that has to be cheap
pub(crate) fn is_render_frame(frame: &Bound<'_, PyAny>) -> PyResult<bool> {
    let state = read();
    let Some(hooks) = &state.hooks else {
        return Ok(false);
    };
    let address = hooks.render.as_ptr() as usize;
    drop(state);
    Ok(frame.getattr("f_code")?.as_ptr() as usize == address)
}

/// resolve an expression the way the template around it would
///
/// django's rules, not python's, and they differ in ways that are usually the
/// bug: `a.b` tries a dictionary key, then an attribute, then a list index, at
/// every step; a name that holds a callable is **called**; and `x|upper` is a
/// filter rather than a `TypeError`
///
/// it is django's own machinery throughout. `FilterExpression` parses the text
/// against a `Parser` carrying the engine's registered libraries and builtins,
/// so a filter the project installed resolves exactly as it does in the
/// template. what bpd adds is one thing: the underlying `Variable` is resolved
/// **first**, so a name the context does not hold comes back as django's own
/// `VariableDoesNotExist` rather than as the engine's `string_if_invalid`,
/// which is `''` by default and would be a debugger reporting an empty string
/// for a variable that is not there
pub(crate) fn resolve_expression<'py>(
    python: Python<'py>,
    node: &Bound<'py, PyAny>,
    context: &Bound<'py, PyAny>,
    expression: &str,
) -> PyResult<Bound<'py, PyAny>> {
    let (parser_class, filter_class) = {
        let state = read();
        let Some(hooks) = &state.hooks else {
            unreachable!(
                "a template frame is only ever synthesised over the \
                 `Node.render_annotated` hook, which is resolved with the rest"
            );
        };
        (
            hooks.parser.clone_ref(python),
            hooks.filter_expression.clone_ref(python),
        )
    };
    let origin = node.getattr("origin")?;

    // the engine the template was loaded by, so its own filter libraries are
    // the ones in scope. `context.template` is bound for the whole of a render;
    // the loader is the same answer by another route, for a context that is
    // being rendered outside one
    let engine = match context.getattr("template").and_then(|template| {
        if template.is_none() {
            Err(pyo3::exceptions::PyAttributeError::new_err(
                "this context is not bound to a template",
            ))
        } else {
            template.getattr("engine")
        }
    }) {
        Ok(engine) => engine,
        Err(_) => origin.getattr("loader")?.getattr("engine")?,
    };

    let parser = parser_class.bind(python).call1((
        pyo3::types::PyList::empty(python),
        engine.getattr("template_libraries")?,
        engine.getattr("template_builtins")?,
        origin,
    ))?;

    let filtered = filter_class.bind(python).call1((expression, parser))?;

    // a literal has no variable to fail, and `is_var` is django's own way of
    // saying which of the two this is
    if filtered.getattr("is_var")?.is_truthy()? {
        filtered
            .getattr("var")?
            .call_method1("resolve", (context,))?;
    }
    filtered.call_method1("resolve", (context,))
}

/// the node and the context a `Node.render_annotated` frame was handed
pub(crate) fn rendered<'py>(
    frame: &Bound<'py, PyAny>,
) -> PyResult<(Bound<'py, PyAny>, Bound<'py, PyAny>)> {
    let locals = frame.getattr("f_locals")?;
    let node = locals.get_item("self")?;
    let context = locals.get_item("context")?;
    Ok((node, context))
}
