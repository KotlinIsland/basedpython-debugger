//! the stopped thread's stack, its scopes, and what can be done to them
//!
//! a frame is materialised **only** at a stop. deciding whether to stop needs a
//! code object's address and a line number and nothing else, which is what
//! makes an uninteresting event cheap — see [`crate::events::current_frame`]
//!
//! ## no frame of bpd's is ever in a stack
//!
//! the agent is native, so the interpreter pushes no python frame to call it
//! and none of the callbacks appear anywhere. there is exactly one python frame
//! that belongs to `bpd`: the `-c` bootstrap the interpreter was entered
//! through, which is the outermost frame of the process and the parent of the
//! program's own module frame. it is remembered when the agent arms itself and
//! the walk stops **at** it, so it is never reported and neither is anything
//! above it
//!
//! ## the scopes are not one namespace
//!
//! `f_locals` merges a frame's locals, the cells a nested function captures
//! from it, and the free variables it captures from an enclosing frame, into
//! one mapping. that mapping is the right thing to *evaluate* against, because
//! it is what `LOAD_NAME` would see — and it is the wrong thing to *report*,
//! because it cannot distinguish a captured variable from a global of the same
//! name. so the names come from the code object, one scope at a time, and the
//! values come from the frame
//!
//! ## what a stop claims about the other threads
//!
//! a stop holds one thread, and every other thread in the process goes on
//! running unless the world has been stopped explicitly. so the held thread's
//! **stack** is a snapshot either way — it is inside a monitoring callback and
//! cannot return, so its frames cannot go away underneath the walk — and
//! everything the frames point *at* is a sample. every answer here carries the
//! mode it was taken in for exactly that reason
//!
//! there is still no request here that walks a thread bpd is not holding: its
//! frames are moving, and a stack read off one would be a description of a
//! moment that has already gone. where a running thread is, stated as the
//! sample it is, is [`crate::threads`]

use std::sync::OnceLock;

use bpd_core::{
    ContextLayer, Detail, Entry, Evaluated, Frame, FrameId, FrameKind, Holding, Jump, Jumped,
    Omitted, Refusal, Scope, Suspendable, Unrestartable, Where,
};
use bpd_protocol::message::FromAgent;
use pyo3::exceptions::PyKeyError;
use pyo3::prelude::*;
use pyo3::types::PyDict;

use crate::conditions::{self, capture};
use crate::facts::Prover;
use crate::values::Reader;
use crate::{events, sources, templates, world};

/// `CO_OPTIMIZED` — the frame keeps its locals in slots the compiler assigned
///
/// true of every function and false of a module or a class body, whose locals
/// are an ordinary namespace mapping. the two are read differently and a
/// debugger that treated them alike would report one of them wrongly
const CO_OPTIMIZED: u32 = 0x1;

/// `CO_GENERATOR`, `CO_COROUTINE`, `CO_ASYNC_GENERATOR` — the three flags for a
/// frame its driver sends into rather than one that is called
///
/// what makes them one group here is the first instruction of the code object:
/// for all three it is the `RESUME` that `send`, `throw` and `await` enter at,
/// rather than the top of the body. restarting such a frame moves to that
/// instruction and ends the frame — see [`Unrestartable::Suspendable`]
const CO_GENERATOR: u32 = 0x20;
const CO_COROUTINE: u32 = 0x80;
const CO_ASYNC_GENERATOR: u32 = 0x200;

/// the frame the agent entered the program from
///
/// held for the life of the process, which costs nothing: it is the outermost
/// frame of the process and is alive for exactly that long anyway
static BOOTSTRAP: OnceLock<Py<PyAny>> = OnceLock::new();

/// the filenames cpython gives the import machinery's own frames
///
/// the import system is frozen into the interpreter and its code objects carry
/// these names. matching on them is how the one lock cpython makes knowable is
/// found — there is no api that says "this thread is importing" — and
/// `the_import_machinery_runs_in_frames_named_after_itself` pins the names in a
/// bare interpreter so a rename in cpython fails a test rather than silently
/// turning the detection off
const IMPORT_MACHINERY: [&str; 2] = [
    "<frozen importlib._bootstrap>",
    "<frozen importlib._bootstrap_external>",
];

/// the import machinery's own frame that knows which module is being imported
const IMPORTING: &str = "_find_and_load";

/// remember the frame the agent was entered from, so no stack ever reports it
///
/// called from `main`, where `sys._getframe()` is the bootstrap's own frame:
/// the agent's entry point is a native function and the interpreter pushes no
/// frame to call one
pub(crate) fn remember_bootstrap(python: Python<'_>) -> PyResult<()> {
    let frame = events::current_frame(python)?;
    BOOTSTRAP
        .set(frame.unbind())
        .map_err(|_| pyo3::exceptions::PyRuntimeError::new_err("the agent was entered twice"))
}

/// one entry of a stack, which is not always a frame the interpreter has
///
/// a django template is not compiled to python, so the only way a template line
/// can appear in a stack at all is for bpd to synthesise an entry for it. the
/// synthesis is not a guess: every `Node.render_annotated` frame in the walk is
/// one template frame, over the node that frame was handed, so
/// `{% include %}` and `{% extends %}` come out as the nested chain they really
/// are
///
/// what must never happen is one being answered as the other, which is why the
/// two are different shapes here and a different [`bpd_core::FrameKind`] in the
/// answer
#[derive(Debug)]
pub(crate) enum Slot<'py> {
    /// a frame the interpreter really has
    Python(Bound<'py, PyAny>),

    /// a django template frame, synthesised over the frame rendering it
    Template {
        /// the node being rendered
        node: Bound<'py, PyAny>,
        /// the `Context` it is being rendered against
        context: Bound<'py, PyAny>,
        /// how far down the stack the `Node.render_annotated` frame is
        python: u32,
    },
}

/// one stop, and everything that can be asked about it
///
/// the frames are walked the first time something asks for one and dropped when
/// the stop ends, so a frame id can only name a frame that is still there
pub(crate) struct Stopped<'py> {
    python: Python<'py>,
    stop: u64,
    walked: Option<Vec<Slot<'py>>>,
}

/// begin a stop against the number it was registered under
pub(crate) const fn begin(python: Python<'_>, stop: u64) -> Stopped<'_> {
    Stopped {
        python,
        stop,
        walked: None,
    }
}

/// whether a frame is the one the agent entered the program from
/// the code object of the `-c` command the interpreter was entered through
///
/// the one thing about the bootstrap that outlives its frame: cpython keeps the
/// source of a `-c` command in `linecache`, keyed on the code object, and
/// [`crate::run`] takes that entry out so the program never sees bpd's line
pub(crate) fn bootstrap_code(python: Python<'_>) -> PyResult<Bound<'_, PyAny>> {
    let frame = BOOTSTRAP.get().unwrap_or_else(|| {
        unreachable!("the bootstrap frame is remembered before the program is entered")
    });
    frame.bind(python).getattr("f_code")
}

pub(crate) fn is_bootstrap(frame: &Bound<'_, PyAny>) -> bool {
    BOOTSTRAP
        .get()
        .is_some_and(|entry| frame.is(entry.bind(frame.py())))
}

/// what the thread about to be held is holding that others can wait for
///
/// walked once, at the stop, because that is when it is true. it costs a walk
/// of a stack that is about to be walked anyway, and the stop is already a
/// round trip to the engine, so it is not on any path that has to be cheap
pub(crate) fn holding(python: Python<'_>) -> PyResult<Vec<Holding>> {
    let mut holding = Vec::new();
    let mut importing = None;
    let mut inside_import = false;

    for frame in walk(python)? {
        let code = frame.getattr("f_code")?;
        let file: String = code.getattr("co_filename")?.extract()?;
        if !IMPORT_MACHINERY.contains(&file.as_str()) {
            continue;
        }
        inside_import = true;

        // the outermost `_find_and_load` of the walk is the import the program
        // asked for, and the ones under it are its dependencies. the last one
        // seen wins because the walk is innermost first
        let qualname: String = code.getattr("co_qualname")?.extract()?;
        if qualname == IMPORTING {
            let named = frame.getattr("f_locals")?.get_item("name");
            if let Ok(name) = named {
                importing = name.extract::<String>().ok();
            }
        }
    }

    if inside_import {
        holding.push(Holding::ImportSystem { module: importing });
    }
    Ok(holding)
}

impl<'py> Stopped<'py> {
    /// the stack of the thread that stopped, nearest first
    fn frames(&mut self) -> PyResult<&[Slot<'py>]> {
        if self.walked.is_none() {
            self.walked = Some(stack_of(self.python)?);
        }
        Ok(self.walked.as_ref().expect("the branch above filled it in"))
    }

    /// the stack, or as much of it as was asked for
    pub(crate) fn stack(&mut self, top: Option<u32>) -> PyResult<FromAgent> {
        let stop = self.stop;
        let frames = self.frames()?;
        let depth = frames.len();
        let wanted = top.map_or(depth, |top| depth.min(top as usize));

        let mut described = Vec::with_capacity(wanted);
        for (index, slot) in frames.iter().take(wanted).enumerate() {
            described.push(describe(
                slot,
                FrameId {
                    stop,
                    depth: u32::try_from(index).expect("a stack is not four billion frames deep"),
                },
            )?);
        }
        Ok(FromAgent::Stack {
            frames: described,
            depth,
            mode: world::mode(),
        })
    }

    /// the frame an id names, or the refusal that says why there is none
    ///
    /// the stop half of the id is not checked here, because it is what decided
    /// this stop would be asked at all: a request is routed to the thread the
    /// id's stop is holding, and an id from a stop that has ended is refused
    /// before it ever reaches one
    fn slot(&mut self, id: FrameId) -> PyResult<Result<&Slot<'py>, Refusal>> {
        debug_assert_eq!(
            id.stop, self.stop,
            "a request reached the stop it was not addressed to"
        );
        let frames = self.frames()?;
        let depth = frames.len();
        match frames.get(id.depth as usize) {
            Some(slot) => Ok(Ok(slot)),
            None => Ok(Err(Refusal::NoSuchFrame { frame: id, depth })),
        }
    }

    /// the python frame an id names, refusing when the id names a template one
    ///
    /// a template frame is synthesised and the interpreter has no frame for it,
    /// so there is nothing to read a python scope off and nothing to evaluate
    /// python against. answering from the `Node.render_annotated` frame under
    /// it instead would be reading a variable from another scope entirely,
    /// which is the thing this project exists not to do
    fn frame(
        &mut self,
        id: FrameId,
        wanted: &'static str,
    ) -> PyResult<Result<Bound<'py, PyAny>, Refusal>> {
        Ok(match self.slot(id)? {
            Ok(Slot::Python(frame)) => Ok(frame.clone()),
            Ok(Slot::Template { python, .. }) => Err(Refusal::NotAPythonFrame {
                frame: id,
                wanted: wanted.to_string(),
                python: FrameId {
                    stop: id.stop,
                    depth: *python,
                },
            }),
            Err(reason) => Err(reason),
        })
    }

    /// the source around one frame's current line
    ///
    /// read here rather than in the engine because this is the filesystem the
    /// interpreter read the file from, and because it is the only place the
    /// **code object** is — which is what the file on disk has to be checked
    /// against before a line of it may be shown. see [`crate::source`]
    pub(crate) fn source(&mut self, id: FrameId, around: u32) -> PyResult<FromAgent> {
        let frame = match self.frame(id, "the source around a frame")? {
            Ok(frame) => frame,
            Err(reason) => return Ok(FromAgent::Refused { reason }),
        };
        Ok(FromAgent::Source {
            source: crate::source::around(self.python, &frame, around)?,
        })
    }

    /// what one scope of one frame holds
    ///
    /// read at the deepest whole level the byte budget allows, rather than at
    /// the level asked for until it runs out. the difference is not cosmetic:
    /// a namespace begins with `__builtins__`, so the first way spends the
    /// whole answer on one variable nobody asked about and reports the rest as
    /// missing. when the level is reduced the answer says so
    pub(crate) fn variables(
        &mut self,
        id: FrameId,
        scope: Scope,
        detail: Detail,
    ) -> PyResult<FromAgent> {
        let frame = match self.frame(id, "the variables of a scope")? {
            Ok(frame) => frame,
            Err(reason) => return Ok(FromAgent::Refused { reason }),
        };
        let place = Place::of(&frame)?;

        let asked = detail.depth;
        let mut used = asked;
        let mut read = self.scope(
            &place,
            scope,
            Detail {
                depth: used,
                ..detail
            },
        )?;
        while read.exhausted && used > 0 {
            used -= 1;
            read = self.scope(
                &place,
                scope,
                Detail {
                    depth: used,
                    ..detail
                },
            )?;
        }

        let mut omitted = read.omitted;
        if used < asked {
            omitted.push(Omitted::Shallower { asked, used });
        }
        if read.exhausted {
            omitted.push(Omitted::Budget {
                limit: detail.budget,
            });
        }

        Ok(FromAgent::Variables {
            frame: id,
            scope,
            entries: read.entries,
            unbound: read.unbound,
            unreadable: read.unreadable,
            omitted,
            mode: world::mode(),
        })
    }

    /// what is provable about some of a frame's names, and for how long
    ///
    /// the names are resolved the way python resolves them — the first scope of
    /// the frame that holds one wins, in the order the compiler would search —
    /// rather than out of the merged `f_locals`, for the reason the scopes are
    /// reported separately at all
    ///
    /// every name asked about comes back in exactly one of the two lists. a
    /// name that produced no facts and no reason would be indistinguishable
    /// from one bound to something uninteresting
    pub(crate) fn facts(
        &mut self,
        id: FrameId,
        names: Vec<String>,
        limit: bpd_core::Limit,
    ) -> PyResult<FromAgent> {
        let frame = match self.frame(id, "what is provable about a frame's names")? {
            Ok(frame) => frame,
            Err(reason) => return Ok(FromAgent::Refused { reason }),
        };
        let place = Place::of(&frame)?;
        let prover = Prover::new(self.python, limit);

        let mut proved = Vec::new();
        let mut silent = Vec::new();
        for name in names {
            match Self::prove(&place, &prover, &name, limit)? {
                Ok(facts) => proved.extend(facts),
                Err(why) => silent.push(bpd_core::Silent { name, why }),
            }
        }

        Ok(FromAgent::Facts {
            frame: id,
            proved,
            silent,
            mode: world::mode(),
        })
    }

    /// everything provable about one name or dotted path
    fn prove(
        place: &Place<'py>,
        prover: &Prover<'py>,
        name: &str,
        limit: bpd_core::Limit,
    ) -> PyResult<Result<Vec<bpd_core::Fact>, bpd_core::Silence>> {
        let mut segments = name.split('.');
        let root = segments.next().unwrap_or(name);
        let rest: Vec<&str> = segments.collect();

        let segments = rest.len() + 1;
        if segments > limit.depth as usize {
            return Ok(Err(bpd_core::Silence::TooDeep {
                segments,
                limit: limit.depth,
            }));
        }

        let Some(scope) = place.scopes_holding(root)?.into_iter().next() else {
            return Ok(Err(bpd_core::Silence::Unbound));
        };
        let Held::Value(value) = place.read(scope, root)? else {
            return Ok(Err(bpd_core::Silence::Unbound));
        };

        let value = match prover.follow(&value, &rest)? {
            Ok(value) => value,
            Err(why) => return Ok(Err(why)),
        };
        Ok(Ok(prover.about(&value, name, scope)?))
    }

    /// one pass over a scope at one depth
    fn scope(&self, place: &Place<'py>, scope: Scope, detail: Detail) -> PyResult<Read> {
        let mut reader = Reader::new(self.python, detail);
        let mut read = Read::default();

        match place.names(scope) {
            // a scope the code object names one variable at a time: the names
            // are the code object's and the values are the frame's
            Some(names) => {
                for name in names {
                    match place.read(scope, &name)? {
                        Held::Value(value) => read.entries.push(Entry {
                            value: reader.read(&value, &name)?,
                            name,
                        }),
                        Held::Unbound => read.unbound.push(name),
                        Held::Unreadable => read.unreadable.push(name),
                    }
                }
            }
            // a whole namespace mapping — a module, a class body, or the
            // globals of anything
            None => {
                let namespace = place.namespace(scope);
                match namespace.cast::<PyDict>() {
                    Ok(mapping) => {
                        let (entries, omitted) =
                            reader.named(&mapping.items(), "", detail.depth)?;
                        read.entries = entries;
                        read.omitted.extend(omitted);
                    }
                    Err(_) => read.omitted.push(Omitted::NotADictionary),
                }
            }
        }

        read.exhausted = reader.exhausted();
        Ok(read)
    }

    /// what a template frame's django context holds, layer by layer
    ///
    /// never merged. `django.template.Context` is a stack of dicts and django
    /// resolves a name by walking them from the last backwards, so which layer
    /// holds a name is what decides the render — and a merged mapping is a
    /// report in which that has already happened invisibly
    ///
    /// the byte budget is one budget for the **whole** context rather than one
    /// per layer, and it is retried shallower the same way a scope is: a
    /// context whose outermost layer is large would otherwise spend the answer
    /// there and report the layer the user is looking at as missing
    pub(crate) fn template_context(&mut self, id: FrameId, detail: Detail) -> PyResult<FromAgent> {
        let context = match self.slot(id)? {
            Ok(Slot::Template { context, .. }) => context.clone(),
            Ok(Slot::Python(frame)) => {
                let function = frame.getattr("f_code")?.getattr("co_qualname")?.extract()?;
                return Ok(FromAgent::Refused {
                    reason: Refusal::NotATemplateFrame {
                        frame: id,
                        function,
                    },
                });
            }
            Err(reason) => return Ok(FromAgent::Refused { reason }),
        };

        let dicts = context.getattr("dicts")?;
        let asked = detail.depth;
        let mut used = asked;
        let (mut layers, mut exhausted) = self.layers(&dicts, used, detail)?;
        while exhausted && used > 0 {
            used -= 1;
            (layers, exhausted) = self.layers(&dicts, used, detail)?;
        }

        if used < asked || exhausted {
            let last = layers
                .last_mut()
                .expect("a django context always has at least the builtins layer");
            if used < asked {
                last.omitted.push(Omitted::Shallower { asked, used });
            }
            if exhausted {
                last.omitted.push(Omitted::Budget {
                    limit: detail.budget,
                });
            }
        }

        Ok(FromAgent::TemplateContext {
            frame: id,
            layers,
            mode: world::mode(),
        })
    }

    /// one pass over every layer of a context, at one depth
    fn layers(
        &self,
        dicts: &Bound<'py, PyAny>,
        depth: u32,
        detail: Detail,
    ) -> PyResult<(Vec<ContextLayer>, bool)> {
        let mut reader = Reader::new(self.python, detail);
        let mut layers = Vec::new();

        for (index, layer) in dicts.try_iter()?.enumerate() {
            let layer = layer?;
            let index = u32::try_from(index).expect("a context is not four billion layers deep");
            let mut omitted = Vec::new();
            let entries = match layer.cast::<PyDict>() {
                Ok(mapping) => {
                    let (entries, cut) = reader.named(&mapping.items(), "", depth)?;
                    omitted.extend(cut);
                    entries
                }
                // django pushes dicts, and a `RequestContext` processor that
                // pushed something else would be read as the mapping it is not.
                // saying so is the same answer a module namespace that is not a
                // dictionary gets
                Err(_) => {
                    omitted.push(Omitted::NotADictionary);
                    Vec::new()
                }
            };
            layers.push(ContextLayer {
                index,
                entries,
                omitted,
            });
        }

        let exhausted = reader.exhausted();
        Ok((layers, exhausted))
    }

    /// evaluate an expression in a frame
    pub(crate) fn evaluate(
        &mut self,
        id: FrameId,
        expression: &str,
        detail: Detail,
    ) -> PyResult<FromAgent> {
        // the frame decides the language. against a template frame the text is
        // template syntax and django resolves it, because that is what the same
        // text means where the user is looking — `user.profile.name` is a
        // dictionary key before it is an attribute, and `called` is the result
        // of calling it. python in a template frame is reached by naming the
        // python frame underneath, which the template frame carries
        if let Ok(Slot::Template { node, context, .. }) = self.slot(id)? {
            let (node, context) = (node.clone(), context.clone());
            let result =
                match templates::resolve_expression(self.python, &node, &context, expression) {
                    Ok(value) => Evaluated::Value {
                        value: Reader::new(self.python, detail).read(&value, expression)?,
                    },
                    Err(error) => Evaluated::Raised {
                        error: capture(self.python, &error),
                    },
                };
            return Ok(FromAgent::Evaluated {
                result,
                mode: world::mode(),
            });
        }

        let frame = match self.frame(id, "evaluating a python expression")? {
            Ok(frame) => frame,
            Err(reason) => return Ok(FromAgent::Refused { reason }),
        };
        let place = Place::of(&frame)?;

        let result = match place.evaluate(self.python, expression) {
            Ok(value) => Evaluated::Value {
                value: Reader::new(self.python, detail).read(&value, expression)?,
            },
            Err(error) => Evaluated::Raised {
                error: capture(self.python, &error),
            },
        };
        Ok(FromAgent::Evaluated {
            result,
            mode: world::mode(),
        })
    }

    /// move the executing frame to another line of the code it is running
    pub(crate) fn set_next_statement(&mut self, id: FrameId, line: u32) -> PyResult<FromAgent> {
        self.jump(id, Wanted::Line(line), "setting the next statement")
    }

    /// re-enter the executing frame from the top
    pub(crate) fn restart_frame(&mut self, id: FrameId) -> PyResult<FromAgent> {
        self.jump(id, Wanted::FirstLine, "restarting a frame")
    }

    /// the frame an id names, when it is the one its thread is executing
    ///
    /// a jump is only sound in that frame. every frame below it is suspended in
    /// a call, and cpython does **not** refuse a move in one — measured on 3.13,
    /// 3.14 and 3.15, the assignment is accepted and the frame goes on with a
    /// value stack that no longer matches where it is, so the function returns
    /// something it never computed. that is the whole reason this check is here
    /// rather than left to the interpreter
    fn executing(
        &mut self,
        id: FrameId,
        wanted: &'static str,
    ) -> PyResult<Result<Bound<'py, PyAny>, Refusal>> {
        let frame = match self.frame(id, wanted)? {
            Ok(frame) => frame,
            Err(reason) => return Ok(Err(reason)),
        };

        let stop = self.stop;
        let depth = self
            .frames()?
            .iter()
            .position(|slot| matches!(slot, Slot::Python(_)))
            .unwrap_or_else(|| {
                unreachable!(
                    "the walk starts at the frame the interpreter is in, so the \
                     stack of a held thread holds at least one python frame"
                )
            });
        let executing = FrameId {
            stop,
            depth: u32::try_from(depth).expect("a stack is not four billion frames deep"),
        };

        if id != executing {
            return Ok(Err(Refusal::NotTheExecutingFrame {
                frame: id,
                executing,
                wanted: wanted.to_string(),
            }));
        }
        debug_assert!(
            frame.is(&events::current_frame(self.python)?),
            "the innermost python frame of a held thread's stack is the frame \
             the interpreter is in"
        );
        Ok(Ok(frame))
    }

    /// move the executing frame, and report what that did to it
    fn jump(&mut self, id: FrameId, wanted: Wanted, what: &'static str) -> PyResult<FromAgent> {
        let frame = match self.executing(id, what)? {
            Ok(frame) => frame,
            Err(reason) => return Ok(FromAgent::Refused { reason }),
        };
        let code = frame.getattr("f_code")?;

        // the line a client names is a line of the file the frame **reported**,
        // and for a basedpython build that is the `.by`. translating it here is
        // the other half of reporting one: a debugger that answered in one
        // file's lines and took orders in another's would be two debuggers
        let asked = match wanted {
            Wanted::Line(line) => line,
            Wanted::FirstLine => match restartable(&code)? {
                Ok(line) => line,
                Err(reason) => {
                    return Ok(FromAgent::Refused {
                        reason: Refusal::NotRestartable {
                            frame: id,
                            function: code.getattr("co_qualname")?.extract()?,
                            reason,
                        },
                    });
                }
            },
        };
        // a restart's destination came out of the code object and is already a
        // line the interpreter has, so only a line a client named is translated
        let line = match wanted {
            Wanted::Line(_) => match translate(&code, asked)? {
                Ok(line) => line,
                Err(reason) => {
                    return Ok(FromAgent::Refused {
                        reason: Refusal::UnmappableLine { frame: id, reason },
                    });
                }
            },
            Wanted::FirstLine => asked,
        };

        // where the frame is now, as a client was told it — the same reading
        // the stack gave, rather than the interpreter's line beside a `.by` one
        let from = describe_where(&frame)?.line;
        // every name of the frame's own slots that holds nothing right now.
        // cpython binds all of them to `None` as part of a jump, which is a
        // change to the program's state that the debugger caused and that
        // nothing else would say
        let unbound = Place::of(&frame)?.unbound()?;

        // the assignment runs the warnings machinery, which is the program's own
        // code — `showwarning` is replaceable and `linecache` reads the file. a
        // breakpoint reached inside it would be a stop whose stack is half
        // debugger, which is what this holds off for the same reason a condition
        // does
        let moved = {
            let _suppressed = conditions::suppress();
            frame.setattr("f_lineno", line)
        };

        // read off the frame rather than assumed from the line that was asked
        // for: no `LINE` event is delivered for the destination, so the frame
        // itself is the only thing that can say where the program is now — and
        // after a refusal it says the same way that nothing moved
        let at = describe_where(&frame)?;
        // the breakpoint table is keyed by the line the interpreter has, so
        // this is read off the frame rather than taken from `at` — which is the
        // same location said the way a client is told it
        let landed: u32 = frame.getattr("f_lineno")?.extract()?;

        let outcome = match moved {
            Ok(()) => Jump::Moved {
                from,
                bound_to_none: bound_to_none(&frame, &unbound)?,
                // against the line the frame is on now rather than the line that
                // was asked for. they are the same line, and one of them is a
                // reading and the other is an expectation
                unannounced: crate::breakpoints::bound_at(code.as_ptr() as usize, landed),
            },
            Err(error) => Jump::Refused {
                // the line the client asked for, in the terms it asked in
                wanted: asked,
                error: capture(self.python, &error),
            },
        };

        Ok(FromAgent::Jumped {
            jumped: Jumped {
                at,
                outcome,
                mode: world::mode(),
            },
        })
    }

    /// write a variable of a frame, and report what the frame holds afterwards
    pub(crate) fn set_variable(
        &mut self,
        id: FrameId,
        scope: Scope,
        name: &str,
        value: &str,
        detail: Detail,
    ) -> PyResult<FromAgent> {
        let frame = match self.frame(id, "writing a variable")? {
            Ok(frame) => frame,
            Err(reason) => return Ok(FromAgent::Refused { reason }),
        };
        let place = Place::of(&frame)?;

        // the name has to be in the scope that was asked for, before anything
        // is evaluated. `f_locals` accepts a write of a name the code object
        // does not have and reads it back, while the program goes on reading
        // the fast local the compiler gave it — so a debugger that skipped this
        // check would report a write the program never received
        let holding = place.scopes_holding(name)?;
        if !holding.contains(&scope) {
            return Ok(FromAgent::Refused {
                reason: Refusal::NoSuchVariable {
                    frame: id,
                    scope,
                    name: name.to_string(),
                    elsewhere: holding,
                },
            });
        }
        if !place.writable(scope) {
            return Ok(FromAgent::Refused {
                reason: Refusal::UnreadableVariable {
                    frame: id,
                    scope,
                    name: name.to_string(),
                },
            });
        }

        let result = match place
            .evaluate(self.python, value)
            .and_then(|new| place.write(scope, name, &new))
        {
            Ok(()) => match place.read(scope, name)? {
                Held::Value(held) => Evaluated::Value {
                    value: Reader::new(self.python, detail).read(&held, name)?,
                },
                held => unreachable!(
                    "`{name}` was written into a scope that holds it and read \
                     back as {held:?}"
                ),
            },
            Err(error) => Evaluated::Raised {
                error: capture(self.python, &error),
            },
        };
        Ok(FromAgent::Evaluated {
            result,
            mode: world::mode(),
        })
    }
}

/// where a jump is going, before the code object has been looked at
///
/// the two operations differ in exactly this and in nothing else
#[derive(Debug, Clone, Copy)]
enum Wanted {
    /// the line the caller named
    Line(u32),
    /// the line of the code object's first instruction that carries one
    FirstLine,
}

/// the line a restart moves to, or why the frame cannot be re-entered
///
/// the destination is the line of the **first instruction** that carries one,
/// in offset order, rather than `co_firstlineno`. the two differ: a module's
/// first instruction is a `RESUME` whose line is `0`, and a function that closes
/// over a variable begins with `MAKE_CELL` instructions that carry no line at
/// all. `co_firstlineno` is where the code was written, and what a jump needs is
/// a position the frame can be put at
/// the line of the interpreter's own file that a named line means
///
/// a frame of generated python is reported as the `.by` line behind it, so the
/// line a client names against that frame is a `.by` line and the interpreter
/// has never heard of it. a frame of anything else is reported as itself, and
/// the line means what it says
fn translate(code: &Bound<'_, PyAny>, line: u32) -> PyResult<Result<u32, bpd_core::Unmapped>> {
    let file: String = code.getattr("co_filename")?.extract()?;
    Ok(sources::to_generated(&file, line).unwrap_or(Ok(line)))
}

fn restartable(code: &Bound<'_, PyAny>) -> PyResult<Result<u32, Unrestartable>> {
    let flags: u32 = code.getattr("co_flags")?.extract()?;
    for (flag, kind) in [
        (CO_GENERATOR, Suspendable::Generator),
        (CO_COROUTINE, Suspendable::Coroutine),
        (CO_ASYNC_GENERATOR, Suspendable::AsyncGenerator),
    ] {
        if flags & flag != 0 {
            return Ok(Err(Unrestartable::Suspendable { kind }));
        }
    }

    let mut first: Option<(u32, u32)> = None;
    for entry in code.call_method0("co_lines")?.try_iter()? {
        let (start, _end, line): (u32, u32, Option<u32>) = entry?.extract()?;
        // `0` is what cpython gives a module's own `RESUME`, and `None` is what
        // it gives an instruction with no source line at all. neither is a line
        // of the file, and neither can be jumped to
        let Some(line) = line.filter(|line| *line >= 1) else {
            continue;
        };
        if first.is_none_or(|(earliest, _)| start < earliest) {
            first = Some((start, line));
        }
    }

    Ok(first.map_or(Err(Unrestartable::NoFirstLine), |(_, line)| Ok(line)))
}

/// which of the names that held nothing before a jump hold `None` after it
///
/// read back out of the frame rather than predicted from the warning cpython
/// raises. the assertion is the invariant that makes the field's name true: if
/// a jump ever binds an unbound local to something other than `None`, a report
/// that called it `bound_to_none` would be a wrong statement about the program
fn bound_to_none(frame: &Bound<'_, PyAny>, unbound: &[(Scope, String)]) -> PyResult<Vec<String>> {
    let place = Place::of(frame)?;
    let mut bound = Vec::new();
    for (scope, name) in unbound {
        if let Held::Value(value) = place.read(*scope, name)? {
            assert!(
                value.is_none(),
                "cpython binds a frame's unbound locals to `None` when it jumps, \
                 and `{name}` came back holding {value}"
            );
            bound.push(name.clone());
        }
    }
    Ok(bound)
}

/// the stack as a client sees it: the python frames, with template frames over
///
/// a template frame goes immediately **above** the `Node.render_annotated`
/// frame that renders it, so depth zero at a template breakpoint is the
/// template line and the frame under it is the python that got there
fn stack_of(python: Python<'_>) -> PyResult<Vec<Slot<'_>>> {
    let frames = walk(python)?;
    let mut slots = Vec::with_capacity(frames.len());

    for frame in frames {
        if templates::is_render_frame(&frame)? {
            let (node, context) = templates::rendered(&frame)?;
            slots.push(Slot::Template {
                node,
                context,
                python: u32::try_from(slots.len() + 1)
                    .expect("a stack is not four billion frames deep"),
            });
        }
        slots.push(Slot::Python(frame));
    }
    Ok(slots)
}

/// walk the chain from the frame that stopped, stopping at the bootstrap
fn walk(python: Python<'_>) -> PyResult<Vec<Bound<'_, PyAny>>> {
    let bootstrap = BOOTSTRAP.get().map(|frame| frame.bind(python));
    let mut frames = Vec::new();
    let mut current = Some(events::current_frame(python)?);

    while let Some(frame) = current {
        if bootstrap.is_some_and(|entry| frame.is(entry)) {
            break;
        }
        let back = frame.getattr("f_back")?;
        frames.push(frame);
        current = (!back.is_none()).then_some(back);
    }
    Ok(frames)
}

/// read one stack entry's location
fn describe(slot: &Slot<'_>, id: FrameId) -> PyResult<Frame> {
    match slot {
        Slot::Python(frame) => {
            let code = frame.getattr("f_code")?;
            // a frame of generated python is reported as the `.by` line behind
            // it, with the location the interpreter really has carried beside
            // it. a frame the build did not generate — the standard library,
            // the `_by_runner.py` shim `by run` starts — comes back untouched,
            // because dressing one as basedpython would be inventing a source
            // file for it
            let at = sources::locate(
                code.getattr("co_filename")?.extract()?,
                frame.getattr("f_lineno")?.extract()?,
            );
            Ok(Frame {
                id,
                file: at.file,
                line: at.line,
                mapping: at.mapping,
                kind: FrameKind::Python {
                    function: code.getattr("co_qualname")?.extract()?,
                    first_line: code.getattr("co_firstlineno")?.extract()?,
                },
            })
        }
        Slot::Template { node, python, .. } => {
            let origin = node.getattr("origin")?;
            Ok(Frame {
                id,
                file: origin.getattr("name")?.extract()?,
                line: node.getattr("token")?.getattr("lineno")?.extract()?,
                // a django template is not compiled to python at all, so there
                // is no generated line for a source map to be about
                mapping: None,
                kind: FrameKind::Template {
                    node: node.get_type().getattr("__name__")?.extract()?,
                    python: FrameId {
                        stop: id.stop,
                        depth: *python,
                    },
                },
            })
        }
    }
}

/// the file and line one frame is at
///
/// what a [`StopReason`](bpd_core::StopReason) carries, for the one stop that is
/// not reported from a monitoring callback: a forked child is held in
/// `after_in_child`, where there is no event to have been handed a location by,
/// and the frame chain of the `os.fork()` caller is what says where it is
pub(crate) fn file_and_line(frame: &Bound<'_, PyAny>) -> PyResult<(String, u32)> {
    let file = frame.getattr("f_code")?.getattr("co_filename")?.extract()?;
    let line = frame.getattr("f_lineno")?.extract()?;
    let at = sources::locate(file, line);
    Ok((at.file, at.line))
}

/// where one frame is, without a frame id — there is no stop behind it
///
/// what a thread bpd is **not** holding gets, because a frame id is a handle
/// that stays valid for a stop and a running thread's frame has no such promise
pub(crate) fn describe_where(frame: &Bound<'_, PyAny>) -> PyResult<Where> {
    let code = frame.getattr("f_code")?;
    // mapped like a frame is, and with nowhere to carry the generated location:
    // a `Where` is a sample of a thread bpd is not holding, and there is no
    // frame id on it to ask anything more about. it is the same location a
    // frame of the same code would report, which is what the rule asks for
    let at = sources::locate(
        code.getattr("co_filename")?.extract()?,
        frame.getattr("f_lineno")?.extract()?,
    );
    Ok(Where {
        file: at.file,
        line: at.line,
        function: code.getattr("co_qualname")?.extract()?,
    })
}

/// one pass over one scope
#[derive(Debug, Default)]
struct Read {
    entries: Vec<Entry>,
    unbound: Vec<String>,
    unreadable: Vec<String>,
    omitted: Vec<Omitted>,
    /// whether the byte budget ran out before the scope was finished
    exhausted: bool,
}

/// what a name turned out to be in a frame
#[derive(Debug)]
enum Held<'py> {
    /// it holds this
    Value(Bound<'py, PyAny>),
    /// it is a name of the frame and holds nothing yet
    Unbound,
    /// it is a name of the frame and the frame does not expose it
    Unreadable,
}

/// one frame, and the namespaces it reads and writes through
struct Place<'py> {
    globals: Bound<'py, PyAny>,
    locals: Bound<'py, PyAny>,
    optimized: bool,
    varnames: Vec<String>,
    cellvars: Vec<String>,
    freevars: Vec<String>,
}

impl<'py> Place<'py> {
    fn of(frame: &Bound<'py, PyAny>) -> PyResult<Self> {
        let code = frame.getattr("f_code")?;
        let flags: u32 = code.getattr("co_flags")?.extract()?;
        Ok(Self {
            globals: frame.getattr("f_globals")?,
            // PEP 667's write-through proxy on every interpreter bpd supports,
            // so this is the frame's own state rather than a copy of it — which
            // is what makes writing a local something the program observes
            locals: frame.getattr("f_locals")?,
            optimized: flags & CO_OPTIMIZED != 0,
            varnames: code.getattr("co_varnames")?.extract()?,
            cellvars: code.getattr("co_cellvars")?.extract()?,
            freevars: code.getattr("co_freevars")?.extract()?,
        })
    }

    /// the names a scope holds, or `None` when the scope is a whole namespace
    ///
    /// a module's or a class body's locals are a mapping rather than slots the
    /// compiler assigned, so there is no name list to read them one at a time
    fn names(&self, scope: Scope) -> Option<Vec<String>> {
        match scope {
            Scope::Local if self.optimized => Some(self.varnames.clone()),
            Scope::Local | Scope::Global => None,
            Scope::Cell => Some(self.cellvars.clone()),
            Scope::Free => Some(self.freevars.clone()),
        }
    }

    /// the mapping a whole-namespace scope reads
    fn namespace(&self, scope: Scope) -> Bound<'py, PyAny> {
        match scope {
            Scope::Global => self.globals.clone(),
            // for a module frame these are the same object, which is what
            // cpython says a module's local scope is
            Scope::Local | Scope::Cell | Scope::Free => self.locals.clone(),
        }
    }

    /// what the frame holds for a name of one of its own scopes
    fn read(&self, scope: Scope, name: &str) -> PyResult<Held<'py>> {
        let namespace = self.namespace(scope);
        match namespace.get_item(name) {
            Ok(value) => Ok(Held::Value(value)),
            Err(error) if error.is_instance_of::<PyKeyError>(namespace.py()) => {
                Ok(if self.optimized || scope == Scope::Global {
                    // the proxy raises this for a local that has not been
                    // assigned yet, which is a state and not an absence
                    Held::Unbound
                } else {
                    // a module or a class body has no deref storage in its
                    // namespace: a class body's free variables live in cells
                    // that only the function object holds
                    Held::Unreadable
                })
            }
            Err(error) => Err(error),
        }
    }

    /// the names of this frame's own slots that hold nothing right now
    ///
    /// only a frame whose locals are slots the compiler assigned has any: a
    /// module or a class body keeps its locals in a namespace mapping, where a
    /// name that is not there is absent rather than unbound, and cpython's jump
    /// binds nothing in one
    fn unbound(&self) -> PyResult<Vec<(Scope, String)>> {
        if !self.optimized {
            return Ok(Vec::new());
        }

        let mut unbound = Vec::new();
        for (scope, names) in [
            (Scope::Local, &self.varnames),
            (Scope::Cell, &self.cellvars),
            (Scope::Free, &self.freevars),
        ] {
            for name in names {
                if matches!(self.read(scope, name)?, Held::Unbound) {
                    unbound.push((scope, name.clone()));
                }
            }
        }
        Ok(unbound)
    }

    /// every scope of this frame that holds `name`
    fn scopes_holding(&self, name: &str) -> PyResult<Vec<Scope>> {
        let mut holding = Vec::new();
        let named = |names: &[String]| names.iter().any(|held| held == name);

        if if self.optimized {
            named(&self.varnames)
        } else {
            self.locals.contains(name)?
        } {
            holding.push(Scope::Local);
        }
        if named(&self.cellvars) {
            holding.push(Scope::Cell);
        }
        if named(&self.freevars) {
            holding.push(Scope::Free);
        }
        if self.globals.contains(name)? {
            holding.push(Scope::Global);
        }
        Ok(holding)
    }

    /// whether a write to this scope is a write the program will see
    const fn writable(&self, scope: Scope) -> bool {
        match scope {
            Scope::Local | Scope::Global => true,
            // on an optimized frame the proxy writes into the cell itself. on
            // anything else the cell is not reachable from the frame, and
            // writing the name into the namespace would leave a value the
            // compiled code never reads
            Scope::Cell | Scope::Free => self.optimized,
        }
    }

    /// write a name of a scope the frame really has
    fn write(&self, scope: Scope, name: &str, value: &Bound<'py, PyAny>) -> PyResult<()> {
        match scope {
            Scope::Global => self.globals.set_item(name, value),
            Scope::Local | Scope::Cell | Scope::Free => self.locals.set_item(name, value),
        }
    }

    /// evaluate an expression against this frame's own namespaces
    ///
    /// `f_locals` is what is handed over rather than one scope of it, because
    /// this is the one place the merged mapping is the right answer: it is what
    /// `LOAD_NAME` sees, and resolving a name any other way is how a debugger
    /// reads a variable from the wrong scope
    fn evaluate(&self, python: Python<'py>, expression: &str) -> PyResult<Bound<'py, PyAny>> {
        // the program's own code runs here, so this thread's breakpoints are
        // held off for the same reason a condition holds them off: a stop
        // inside an expression of the debugger's would be a stop whose stack is
        // half debugger
        let _suppressed = conditions::suppress();
        let code = events::compile_expression(python, expression, "<bpd evaluation>")?;
        events::evaluate(python, &code, &self.globals, &self.locals)
    }
}
