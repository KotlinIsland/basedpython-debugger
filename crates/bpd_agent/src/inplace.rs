//! running a frame again where it stands, rather than making its caller call again
//!
//! the other way a frame is restarted is [`crate::restarts`]: force it out, let
//! the thread go, and rewind the **caller** to the call so that the interpreter
//! builds a fresh frame. that mechanism asks a great deal of the caller — it has
//! to have a statement after the call, make only that one call on the line, and
//! do nothing on it that runs code of the program — and every one of those is a
//! restart somebody asked for and did not get
//!
//! this asks nothing of the caller, because it never touches it. the frame is
//! moved back to the top of its own body and its locals are put back to what a
//! frame the interpreter had just built would hold. the caller stays suspended
//! in its `CALL`, and when the frame does finally return, its value goes where
//! the program was always going to put it
//!
//! so `x = f(f2())` restarts `f` without `f2` running twice, and
//! `x = f(obj.attr)` restarts `f` without the property's getter running twice —
//! not because either is analysed and found safe, but because neither is
//! re-executed at all. the same is true of a frame called from C, one reached by
//! `LOAD_ATTR`, and one whose caller's line makes six other calls
//!
//! ## what it is not
//!
//! **the frame object is the same object.** a fresh call makes a new frame, and
//! this does not: `id(frame)` is unchanged, and anything the program is holding
//! that frame by still holds it. that is a real difference from calling again
//! and it is reported rather than glossed
//!
//! and it is a restart of **one** frame. a frame with live frames above it
//! cannot be reset while they are there — cpython crashes rather than refuses
//! when a frame that is not executing is moved, measured on 3.13, 3.14 and 3.15
//! — so those have to be forced out first, and that is not this function
//!
//! ## the order the two halves go in
//!
//! the jump first, the unbinding second, and never the other way round. cpython
//! binds **every** unbound local of a frame to `None` as part of a move, so a
//! pass made before the jump would leave exactly the slots this exists to clear.
//! measured: `cond` unbound, jumped, and read back as `None`
//!
//! everything that can refuse is decided **before** the jump, including whether
//! the frame's own data can be reached at all. after the jump the frame has
//! moved, and there is no putting it back

use bpd_core::{Reset, Unresettable};
use pyo3::prelude::*;

use crate::{bytecode, frames, interpframe};

/// run `frame` again from the top of its own body
///
/// the caller has already established that this frame is one bpd will restart —
/// that it is executing, that it is the frame the request named, and that the
/// thread is held in it
pub(crate) fn reset(frame: &Bound<'_, PyAny>) -> PyResult<Result<Reset, Unresettable>> {
    let code = frame.getattr("f_code")?;
    let landing = match resettable(frame)? {
        Ok(landing) => landing,
        Err(why) => return Ok(Err(why)),
    };

    let slots = interpframe::nlocalsplus(&code)?;
    let parameters = bytecode::parameter_slots(&code)?;
    let locals = code.getattr("co_varnames")?.len()?;
    let names: Vec<String> = code.getattr("co_varnames")?.extract()?;
    // a parameter is kept because nothing else holds what the call bound it to,
    // and a free variable is kept because a fresh frame would be handed the very
    // same cell by `COPY_FREE_VARS` rather than make a new one. everything
    // between them is a local a fresh frame has not reached yet
    let keep: Vec<bool> = (0..slots)
        .map(|slot| slot < parameters || slot >= locals)
        .collect();

    // read **before** the frame is moved: after the jump its offset is the top
    // of the body, and where it was is what says whether a block was open
    let inside_a_block = bytecode::inside_a_block(&code, frame.getattr("f_lasti")?.extract()?)?;

    // nothing above this point has touched the program. from here it has

    // **bound, so that they can be unbound.** cpython warns — a `RuntimeWarning`
    // naming a count — when a move binds unbound locals to `None`, and that
    // warning is the program's own warnings machinery running, with the
    // program's filters and the program's stderr. every one of these slots is
    // about to be emptied anyway, so binding them first costs nothing and is
    // what leaves the debuggee's output alone
    let place = frames::Place::of(frame)?;
    frames::bind_the_unbound(frame.py(), &place, &place.unbound()?)?;

    let first: u32 = code.getattr("co_firstlineno")?.extract()?;
    frame.setattr("f_lineno", first)?;
    let landed: u32 = frame.getattr("f_lasti")?.extract()?;
    assert_eq!(
        landed, landing,
        "`first_line_not_before` snaps up to the first mark at or after the line \
         asked for, `co_firstlineno` is the lowest line the code object has, and \
         an empty stack is compatible with every stack — so the lowest offset \
         carrying a line is the only place this can land",
    );
    match interpframe::unbind_locals(frame, &keep)? {
        Ok(()) => {}
        Err(why) => unreachable!(
            "the layout calibrated and this frame's data held its own code \
             object before the jump, and a jump changes neither: {why:?}"
        ),
    }

    Ok(Ok(Reset {
        frame: frames::describe_where(frame)?,
        inside_a_block,
        kept: names.iter().take(parameters).cloned().collect(),
        emptied: names
            .iter()
            .enumerate()
            .filter(|(slot, _)| !keep[*slot])
            .map(|(_, name)| name.clone())
            .collect(),
    }))
}

/// whether this frame could be run again where it stands, deciding nothing else
///
/// split from [`reset`] because an unwind has to know the answer **before** it
/// forces anything out: a chain that discovered at the end that the frame it was
/// unwinding to could not be reset would have destroyed the frames above it for
/// nothing
///
/// answers with the offset a reset lands on, which is the one thing the check
/// works out that the act would otherwise work out again
pub(crate) fn resettable(frame: &Bound<'_, PyAny>) -> PyResult<Result<u32, Unresettable>> {
    let code = frame.getattr("f_code")?;

    // in order of how fundamental each is, so that a frame with two reasons is
    // told the one that would still be true if the other were fixed
    if let Some(kind) = frames::suspendable(&code)? {
        return Ok(Err(Unresettable::Suspendable { kind }));
    }
    let cells: Vec<String> = code.getattr("co_cellvars")?.extract()?;
    if let Some(name) = cells.first() {
        return Ok(Err(Unresettable::MakesCells {
            name: name.clone(),
            cells: u32::try_from(cells.len()).expect("a function has few cell variables"),
        }));
    }
    if let Some(name) = bytecode::rebinds_a_parameter(&code)? {
        return Ok(Err(Unresettable::RebindsAParameter { name }));
    }
    if let Err(why) = interpframe::reachable(frame)? {
        return Ok(Err(unreachable_as(why)));
    }
    match bytecode::top_offset(&code)? {
        Some(landing) => Ok(Ok(landing)),
        // a code object whose every range carries no line is one no frame can be
        // sent to the top of, and there is nothing to guess at
        None => Ok(Err(Unresettable::NoTopToReturnTo)),
    }
}

/// whether the frames above `target` can be forced out of its way
///
/// `above` is innermost first. every question here is asked **before** anything
/// is forced out, because a chain that stopped half way would have destroyed
/// frames for a reset that then did not happen
///
/// two different things are asked of each frame, and which depends on where it
/// sits. every frame **receives** a forced return except the innermost, and what
/// its line does with that return before bpd hears about it is
/// [`crate::bytecode::tail_after`]. every frame has to **leave**, and one whose
/// line ends in a return leaves on its own — the rest have to be forced, which
/// takes a clean exit
pub(crate) fn unwindable(
    target: &Bound<'_, PyAny>,
    above: &[Bound<'_, PyAny>],
) -> PyResult<Result<(), Unresettable>> {
    for (index, frame) in above.iter().enumerate() {
        let code = frame.getattr("f_code")?;
        let function: String = code.getattr("co_qualname")?.extract()?;
        if let Some(kind) = frames::suspendable(&code)? {
            return Ok(Err(Unresettable::AFrameAboveIsSuspendable {
                function,
                kind,
            }));
        }
        // the innermost is not suspended in a call — it is where the thread is
        // held — so it has no tail and always has to be forced
        let forcing = if index == 0 {
            true
        } else {
            match tail_of(frame, &code, &function)? {
                Ok(tail) => {
                    if let Some(opcode) = tail.runs {
                        return Ok(Err(Unresettable::ATailWouldRun { function, opcode }));
                    }
                    !tail.returns
                }
                Err(why) => return Ok(Err(why)),
            }
        };
        if forcing && !has_a_clean_exit(frame, &code)? {
            return Ok(Err(Unresettable::AFrameAboveHasNoCleanExit { function }));
        }
    }

    // and the target's own line, which has to both run nothing and reach another
    // line — a frame that returns as soon as the one above it does is never
    // executing again, so there is no moment at which it could be reset
    let code = target.getattr("f_code")?;
    let function: String = code.getattr("co_qualname")?.extract()?;
    match tail_of(target, &code, &function)? {
        Ok(tail) => {
            if let Some(opcode) = tail.runs {
                return Ok(Err(Unresettable::ATailWouldRun { function, opcode }));
            }
            if tail.returns {
                return Ok(Err(Unresettable::NoLineFollowsTheCall { line: tail.line }));
            }
            Ok(Ok(()))
        }
        Err(why) => Ok(Err(why)),
    }
}

/// what this frame's line does after the call it is suspended in comes back
fn tail_of(
    frame: &Bound<'_, PyAny>,
    code: &Bound<'_, PyAny>,
    function: &str,
) -> PyResult<Result<bytecode::Tail, Unresettable>> {
    let lasti: u32 = frame.getattr("f_lasti")?.extract()?;
    Ok(match bytecode::tail_after(code, lasti)? {
        Some(tail) => Ok(tail),
        None => Err(Unresettable::FrameHasNoLine {
            function: function.to_string(),
            lasti,
        }),
    })
}

/// whether this frame has a point it can be moved to that only returns
fn has_a_clean_exit(frame: &Bound<'_, PyAny>, code: &Bound<'_, PyAny>) -> PyResult<bool> {
    Ok(
        match bytecode::exit_tails(code, &frames::namespaces_of(frame)?)? {
            Ok(exits) => !exits.is_empty(),
            Err(_) => false,
        },
    )
}

/// a layout that could not be reached, in the words the client is told
fn unreachable_as(why: interpframe::Unreachable) -> Unresettable {
    match why {
        interpframe::Unreachable::Uncalibrated => Unresettable::LayoutUnknown,
        interpframe::Unreachable::NotItsOwnCode => Unresettable::LayoutNotThisFrame,
    }
}
