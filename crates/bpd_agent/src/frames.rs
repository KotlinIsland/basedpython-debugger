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
//! nothing. only the thread that hit the event is held, so the others still
//! have frames of their own that are moving. there is no request here that
//! reports them, because a stack read off a running thread is a description of
//! a moment that has already gone

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use bpd_protocol::message::{
    Detail, Entry, Evaluated, Frame, FrameId, FromAgent, Omitted, Refusal, Scope,
};
use pyo3::exceptions::PyKeyError;
use pyo3::prelude::*;
use pyo3::types::PyDict;

use crate::conditions::{self, capture};
use crate::events;
use crate::values::Reader;

/// `CO_OPTIMIZED` — the frame keeps its locals in slots the compiler assigned
///
/// true of every function and false of a module or a class body, whose locals
/// are an ordinary namespace mapping. the two are read differently and a
/// debugger that treated them alike would report one of them wrongly
const CO_OPTIMIZED: u32 = 0x1;

/// the frame the agent entered the program from
///
/// held for the life of the process, which costs nothing: it is the outermost
/// frame of the process and is alive for exactly that long anyway
static BOOTSTRAP: OnceLock<Py<PyAny>> = OnceLock::new();

/// how many stops there have been
static STOPS: AtomicU64 = AtomicU64::new(0);

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

/// one stop, and everything that can be asked about it
///
/// the frames are walked the first time something asks for one and dropped when
/// the stop ends, so a frame id can only name a frame that is still there
pub(crate) struct Stopped<'py> {
    python: Python<'py>,
    stop: u64,
    walked: Option<Vec<Bound<'py, PyAny>>>,
}

/// begin a stop, taking the next stop number
pub(crate) fn begin(python: Python<'_>) -> Stopped<'_> {
    Stopped {
        python,
        stop: STOPS.fetch_add(1, Ordering::Relaxed) + 1,
        walked: None,
    }
}

impl<'py> Stopped<'py> {
    /// the frames of the thread that stopped, nearest first
    fn frames(&mut self) -> PyResult<&[Bound<'py, PyAny>]> {
        if self.walked.is_none() {
            self.walked = Some(walk(self.python)?);
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
        for (index, frame) in frames.iter().take(wanted).enumerate() {
            described.push(describe(
                frame,
                FrameId {
                    stop,
                    depth: u32::try_from(index).expect("a stack is not four billion frames deep"),
                },
            )?);
        }
        Ok(FromAgent::Stack {
            frames: described,
            depth,
        })
    }

    /// the frame an id names, or the refusal that says why there is none
    fn frame(&mut self, id: FrameId) -> PyResult<Result<Bound<'py, PyAny>, Refusal>> {
        if id.stop != self.stop {
            return Ok(Err(Refusal::StaleFrame {
                frame: id,
                stop: self.stop,
            }));
        }
        let frames = self.frames()?;
        match frames.get(id.depth as usize) {
            Some(frame) => Ok(Ok(frame.clone())),
            None => Ok(Err(Refusal::NoSuchFrame {
                frame: id,
                depth: frames.len(),
            })),
        }
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
        let frame = match self.frame(id)? {
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
        })
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

    /// evaluate an expression in a frame
    pub(crate) fn evaluate(
        &mut self,
        id: FrameId,
        expression: &str,
        detail: Detail,
    ) -> PyResult<FromAgent> {
        let frame = match self.frame(id)? {
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
        Ok(FromAgent::Evaluated { result })
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
        let frame = match self.frame(id)? {
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
        Ok(FromAgent::Evaluated { result })
    }
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

/// read one frame's location
fn describe(frame: &Bound<'_, PyAny>, id: FrameId) -> PyResult<Frame> {
    let code = frame.getattr("f_code")?;
    Ok(Frame {
        id,
        file: code.getattr("co_filename")?.extract()?,
        line: frame.getattr("f_lineno")?.extract()?,
        function: code.getattr("co_qualname")?.extract()?,
        first_line: code.getattr("co_firstlineno")?.extract()?,
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
