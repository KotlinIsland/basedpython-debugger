//! stopping where an exception is raised, and where one leaves the program
//!
//! two settings, and they answer different questions, because cpython only lets
//! them be different questions
//!
//! ## raised is knowable, and it is knowable once
//!
//! `RAISE` fires when an exception is set in a frame — and cpython fires it
//! **again in every frame the exception propagates into**, with the same
//! exception object, as it looks for a handler. so a naive "stop on every
//! raise" stops once per frame of the stack for one `raise` statement
//!
//! measured rather than assumed:
//! `the_interpreter_raises_an_exception_event_in_every_frame_it_passes_through`
//! runs it in a bare interpreter. what bpd reports is where an exception is
//! **raised**, which is the frame the `raise` is in and the point at which the
//! whole stack is still standing — and not the frames it propagates into
//!
//! the same object can be raised more than once. a program that keeps an
//! exception and raises it in a loop raises it every time round, and a debugger
//! that remembered the object rather than the raise would report the first and
//! miss the rest. so the event is read with the instruction it names: the same
//! object arriving at a `RAISE_VARARGS` is a `raise` statement of the program
//! and a new stop, and arriving anywhere else — at the call that ran the frame
//! it came out of — is propagation. a bare `raise` is a `RERAISE` event, which
//! is not listened for, and is a continuation the same way
//!
//! what that cannot tell apart is C code raising a kept object again: a
//! coroutine's exception stored by asyncio's task and raised into the awaiting
//! coroutine arrives at the `await`, with no `raise` statement to point at.
//! `an_object_c_code_raises_again_is_reported_as_the_same_exception_propagating`
//! pins it
//!
//! the exception a thread last reported is held by a strong reference for as
//! long as it is the last one. a pointer would be cheaper and wrong: a freed
//! object's address is handed straight back to the next one, and a new
//! exception at the old address would be read as the old one still propagating
//!
//! ## uncaught is not knowable at the raise, and is not guessed
//!
//! whether an exception will be caught is decided by what happens after it is
//! raised. a debugger that answered at the raise would be scanning exception
//! tables and predicting, and a wrong prediction here is a stop that says
//! "nothing will catch this" about something a library catches a frame later
//!
//! so it is answered where it is known: at the `PY_UNWIND` that takes the
//! exception out of a frame with no caller bpd would report. the cost of
//! knowing rather than predicting is that the frames it came through have
//! already been popped — what is left of them is the exception's own traceback,
//! which is what the stop carries
//!
//! **an exception that escapes a `threading.Thread`'s target is not uncaught**,
//! and is not reported as one: `threading` catches it in `_bootstrap_inner` and
//! hands it to `threading.excepthook`. that is cpython's behaviour rather than
//! a limit of this design, and it is
//! `an_exception_a_worker_thread_lets_escape_is_caught_by_threading_itself`

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};

use bpd_core::{ClassFound, ExceptionClass};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyModule};

include!(concat!(env!("OUT_DIR"), "/opcodes.rs"));

/// stop where an exception is raised
static RAISED: AtomicBool = AtomicBool::new(false);

/// stop where an exception leaves the outermost frame
static UNCAUGHT: AtomicBool = AtomicBool::new(false);

/// whether any class's rule stops where one is raised
static ANY_RAISED: AtomicBool = AtomicBool::new(false);

/// whether any class's rule stops where one leaves the program
static ANY_UNCAUGHT: AtomicBool = AtomicBool::new(false);

/// the rules for single classes
///
/// shared rather than copied out: `decide` runs in the `RAISE` callback of
/// every frame an exception passes through, and takes the rules without
/// holding the lock while it calls into the interpreter
static RULES: std::sync::Mutex<Option<std::sync::Arc<[ExceptionClass]>>> =
    std::sync::Mutex::new(None);

fn rules() -> std::sync::Arc<[ExceptionClass]> {
    RULES
        .lock()
        .expect("nothing panics holding the exception rules: every path through them is a copy")
        .clone()
        .unwrap_or_else(|| std::sync::Arc::from([]))
}

thread_local! {
    /// the exception this thread was last stopped for
    ///
    /// held, not pointed at, so an address that came round again cannot be
    /// mistaken for the exception that used to live there
    static REPORTED: RefCell<Option<Py<PyAny>>> = const { RefCell::new(None) };
}

/// whether a raise should stop the thread that made it
pub(crate) fn raised() -> bool {
    RAISED.load(Ordering::Relaxed)
}

/// whether an exception leaving the outermost frame should stop its thread
pub(crate) fn uncaught() -> bool {
    UNCAUGHT.load(Ordering::Relaxed)
}

/// set both, together with the rules for single classes, because the request
/// carries all three
pub(crate) fn watch(raised: bool, uncaught: bool, classes: Vec<ExceptionClass>) {
    RAISED.store(raised, Ordering::Relaxed);
    UNCAUGHT.store(uncaught, Ordering::Relaxed);
    ANY_RAISED.store(classes.iter().any(|rule| rule.raised), Ordering::Relaxed);
    ANY_UNCAUGHT.store(classes.iter().any(|rule| rule.uncaught), Ordering::Relaxed);
    *RULES
        .lock()
        .expect("nothing panics holding the exception rules: every path through them is a copy") =
        Some(classes.into());
}

/// the rules for single classes, as they were set
pub(crate) fn classes() -> Vec<ExceptionClass> {
    rules().to_vec()
}

/// whether `RAISE` has to be armed: the filter, or any class's rule
pub(crate) fn raised_armed() -> bool {
    RAISED.load(Ordering::Relaxed) || ANY_RAISED.load(Ordering::Relaxed)
}

/// whether an exception leaving the program has to be watched for
pub(crate) fn uncaught_armed() -> bool {
    UNCAUGHT.load(Ordering::Relaxed) || ANY_UNCAUGHT.load(Ordering::Relaxed)
}

/// what was decided about stopping for one exception
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Decision {
    /// it is not a stop
    Pass,
    /// it is a stop, and `libraries` says whether a library raising it counts
    /// or whether it is reported where it arrives in the program's own code
    Stop {
        /// whether a library's raise is a stop where it was raised
        libraries: bool,
    },
}

/// whether this exception is a stop, at a raise or at the program's edge
///
/// the rule for the **nearest** class in the exception's method resolution
/// order decides — so a rule for `KeyError` overrides one for `LookupError`,
/// as `except KeyError:` before `except LookupError:` would — and an exception
/// no rule names is the filter's. the filter's library rule is just-my-code
pub(crate) fn decide(exception: &Bound<'_, PyAny>, at_the_edge: bool) -> PyResult<Decision> {
    let rules = rules();
    if !rules.is_empty() {
        let mro = crate::values::type_own(exception.get_type().as_any(), "__mro__")?;
        for class in mro.try_iter()? {
            let name = class_name(&class?)?;
            if let Some(rule) = rules.iter().find(|rule| rule.name == name) {
                let stops = if at_the_edge {
                    rule.uncaught
                } else {
                    rule.raised
                };
                return Ok(if stops {
                    Decision::Stop {
                        libraries: rule.libraries,
                    }
                } else {
                    Decision::Pass
                });
            }
        }
    }
    let stops = if at_the_edge { uncaught() } else { raised() };
    Ok(if stops {
        Decision::Stop {
            libraries: !crate::libraries::on(),
        }
    } else {
        Decision::Pass
    })
}

/// the name a rule matches a class by: `module.QualName`, or a builtin's bare
/// `QualName`
///
/// read through `type`'s own descriptors, so a metaclass's `__getattribute__`
/// is not run to find out what a class is called — this runs in the `RAISE`
/// callback, once per class of an exception's method resolution order
fn class_name(class: &Bound<'_, PyAny>) -> PyResult<String> {
    let module: String = crate::values::type_own(class, "__module__")?.extract()?;
    let qualname: String = crate::values::type_own(class, "__qualname__")?.extract()?;
    Ok(if module == "builtins" {
        qualname
    } else {
        format!("{module}.{qualname}")
    })
}

/// what a rule's name names in the program now
///
/// every way of reading the name as a module and a qualified name inside it is
/// tried, the longest module first: `a.b.C` is `C` in `a.b`, or `b.C` in `a`. a
/// module is only looked for in `sys.modules`, and a name inside one only in
/// its namespace and the namespaces of the classes under it, so nothing is
/// imported and no `__getattr__` of the program's is run to answer. a name with
/// no module is a builtin's, because that is how [`decide`] names one
pub(crate) fn found(python: Python<'_>, name: &str) -> PyResult<ClassFound> {
    let parts: Vec<&str> = name.split('.').collect();
    if parts.iter().any(|part| part.is_empty()) {
        return Ok(ClassFound::Unresolved {
            why: format!("`{name}` is not a dotted name, so nothing can be called it"),
        });
    }
    let readings: Vec<(String, &[&str])> = if parts.len() == 1 {
        vec![(String::from("builtins"), &parts[..])]
    } else {
        (1..parts.len())
            .rev()
            .map(|split| (parts[..split].join("."), &parts[split..]))
            .collect()
    };

    let modules = PyModule::import(python, "sys")?.getattr("modules")?;
    let mut missed = Vec::new();
    for (module, inside) in readings {
        let Some(held) = modules
            .get_item(&module)
            .ok()
            .filter(|held| !held.is_none())
        else {
            missed.push(format!("`{module}` is not imported"));
            continue;
        };
        let Ok(held) = held.cast::<PyModule>() else {
            missed.push(format!("`{module}` in `sys.modules` is not a module"));
            continue;
        };
        let Some(object) = within(&held.dict().into_any(), inside)? else {
            missed.push(format!("`{module}` has no `{}`", inside.join(".")));
            continue;
        };
        let Ok(class) = object.cast::<pyo3::types::PyType>() else {
            let kind = object
                .get_type()
                .fully_qualified_name()?
                .to_string_lossy()
                .into_owned();
            return Ok(ClassFound::NotAnException {
                what: format!("a `{kind}`, not a class"),
            });
        };
        if !class.is_subclass_of::<pyo3::exceptions::PyBaseException>()? {
            return Ok(ClassFound::NotAnException {
                what: String::from("a class that does not derive from `BaseException`"),
            });
        }
        let named = class_name(class.as_any())?;
        return Ok(if named == name {
            ClassFound::Class
        } else {
            ClassFound::Renamed { named }
        });
    }
    Ok(ClassFound::Unresolved {
        why: missed.join("; "),
    })
}

/// the object a qualified name names inside a namespace, if it names one
///
/// a class's namespace is read through `type`'s own `__dict__`, so a
/// metaclass is not asked
fn within<'py>(
    namespace: &Bound<'py, PyAny>,
    qualname: &[&str],
) -> PyResult<Option<Bound<'py, PyAny>>> {
    let mut namespace = namespace.clone();
    let mut object = None;
    for part in qualname {
        if let Some(outer) = &object {
            let outer: &Bound<'py, PyAny> = outer;
            if !outer.is_instance_of::<pyo3::types::PyType>() {
                return Ok(None);
            }
            namespace = crate::values::type_own(outer, "__dict__")?;
        }
        let Ok(found) = namespace.get_item(*part) else {
            return Ok(None);
        };
        object = Some(found);
    }
    Ok(object)
}

/// whether this `RAISE` event is an exception being raised, rather than one
/// this thread has already been stopped for propagating into another frame
///
/// the propagation of one exception raises the event once per frame with the
/// same object. a `raise` statement raising that object again is a new raise,
/// and the instruction the event names is what says which this is
pub(crate) fn newly_raised(
    python: Python<'_>,
    code: &Bound<'_, PyAny>,
    offset: i32,
    exception: &Bound<'_, PyAny>,
) -> PyResult<bool> {
    let seen_before = REPORTED.with(|cell| {
        cell.borrow()
            .as_ref()
            .is_some_and(|last| exception.is(last.bind(python)))
    });
    if seen_before && !raised_by_a_statement(code, offset)? {
        return Ok(false);
    }
    REPORTED.with(|cell| *cell.borrow_mut() = Some(exception.clone().unbind()));
    Ok(true)
}

/// whether the instruction a `RAISE` event names is a `raise` statement
///
/// `co_code` rather than the adaptive bytecode: `RAISE_VARARGS` has no
/// specialised form, but the interpreter's own copy is the one that could gain
/// one, and `co_code` is the one it promises is the compiled program
fn raised_by_a_statement(code: &Bound<'_, PyAny>, offset: i32) -> PyResult<bool> {
    let bytecode = code.getattr("co_code")?;
    let bytecode = bytecode.cast::<PyBytes>()?;
    let Ok(offset) = usize::try_from(offset) else {
        unreachable!(
            "a RAISE event named instruction offset {offset}, and offsets are not negative"
        );
    };
    let Some(opcode) = bytecode.as_bytes().get(offset) else {
        unreachable!(
            "a RAISE event named instruction offset {offset} in a code object of {} bytes",
            bytecode.len()?
        );
    };
    Ok(*opcode == RAISE_VARARGS)
}
