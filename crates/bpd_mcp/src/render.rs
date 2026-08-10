//! turning what a session answered into the json an agent reads
//!
//! two rules run through all of it, and they are the same rule:
//!
//! - **nothing is elided silently.** every bound that bit is carried as the
//!   core's own sentence about it, which names what was left out, how much of it
//!   there was, and which field to raise. an agent cannot see the ellipsis a
//!   person would
//! - **structure and prose, not one or the other.** the machine readable form is
//!   the core type's own serde, so nothing is dropped on the way; the prose
//!   beside it is the core type's own `Display`, so there is one wording rather
//!   than one per front end

use bpd_core::{
    Binding, Evaluated, LogRecord, Resolved, SourceBreakpoint, Stack, Stop, Threads, Transcript,
    Variables, WorldStopped,
};

/// one held thread, as the answer to a control tool
pub fn stop(stopped: &Stop) -> serde_json::Value {
    serde_json::json!({
        "stop": stopped.stop,
        "thread": stopped.thread,
        "reason": stopped.reason,
        // what this thread holds that another can be waiting for. empty means
        // nothing bpd can know about was held, **not** that nothing was
        "holding": stopped
            .holding
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<String>>(),
    })
}

/// one held thread's stack
pub fn stack(walked: &Stack) -> serde_json::Value {
    let frames: Vec<serde_json::Value> = walked
        .frames
        .iter()
        .map(|frame| {
            serde_json::json!({
                "frame": frame.id.depth,
                "file": frame.file,
                "line": frame.line,
                "function": frame.function,
                "first_line": frame.first_line,
            })
        })
        .collect();

    let mut rendered = serde_json::json!({
        "frames": frames,
        "depth": walked.depth,
        "mode": walked.mode.to_string(),
    });
    if walked.frames.len() < walked.depth {
        rendered["frames_omitted"] = serde_json::json!({
            "count": walked.depth - walked.frames.len(),
            "says": format!(
                "the walk was bounded at {} frames and the stack is {} deep. \
                 ask again with a larger `top`, or none at all for the whole \
                 stack",
                walked.frames.len(),
                walked.depth
            ),
        });
    }
    rendered
}

/// what one scope of one frame holds
pub fn variables(read: &Variables) -> serde_json::Value {
    serde_json::json!({
        "entries": read.entries,
        // a name the scope has and the frame does not hold yet, and a name whose
        // value the frame does not expose. neither is a value and neither is
        // absent, so neither is left out
        "unbound": read.unbound,
        "unreadable": read.unreadable,
        "left_out": read
            .omitted
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<String>>(),
        "mode": read.mode.to_string(),
    })
}

/// what an expression did, or what a write left behind
///
/// an expression that raised has an answer and the answer is the exception, so
/// this is a result rather than a failure
pub fn evaluated(result: &Evaluated) -> serde_json::Value {
    serde_json::json!({ "result": result })
}

/// what every thread of the debuggee was doing
pub fn threads(census: &Threads) -> serde_json::Value {
    serde_json::json!({
        "threads": census.threads,
        "settle_ms": u64::try_from(census.settle.as_millis()).unwrap_or(u64::MAX),
        "mode": census.mode.to_string(),
        "note": "everything here about a thread bpd is not holding is a sample \
                 taken twice, `settle_ms` apart. `still` means it was in the \
                 same place both times, which is where to look rather than what \
                 is wrong",
    })
}

/// what stopping the world managed to stop
pub fn world(stopped: &WorldStopped) -> serde_json::Value {
    let mut rendered = serde_json::json!({
        "held": stopped.held,
        "native": stopped.native,
        "whole_program": stopped.native.is_empty(),
    });
    if !stopped.native.is_empty() {
        rendered["note"] = format!(
            "{} thread(s) could not be held and are still running: they are \
             parked in a C call, where there is no monitoring event to hold one \
             at. a read taken now is not a whole-program snapshot",
            stopped.native.len()
        )
        .into();
    }
    rendered
}

/// how every breakpoint of a set resolved
///
/// `requested` is what was asked for, by the same index. a breakpoint that moved
/// says which line it moved to, and one that did not bind says why in the core's
/// own words — never "set"
pub fn breakpoints(
    resolved: &[Resolved],
    requested: &[SourceBreakpoint],
) -> Vec<serde_json::Value> {
    resolved
        .iter()
        .map(|entry| {
            let asked = requested.iter().find(|wanted| wanted.id == entry.id);
            let mut rendered = serde_json::json!({ "id": entry.id });
            if let Some(asked) = asked {
                rendered["file"] = asked.file.display().to_string().into();
                rendered["requested_line"] = asked.line.into();
            }
            match &entry.binding {
                Binding::Bound {
                    line,
                    sites,
                    evaluation,
                } => {
                    rendered["bound"] = true.into();
                    rendered["line"] = (*line).into();
                    rendered["evaluation"] = serde_json::json!(evaluation);
                    // one source line can belong to several code objects — a
                    // `def` line is in the class body and is the method's first
                    // line — and every one of them is armed
                    rendered["sites"] = serde_json::json!(sites);
                    if asked.is_some_and(|asked| asked.line != *line) {
                        rendered["moved"] = format!(
                            "line {} is not executable, so this moved to line \
                             {line}, which is",
                            asked.map_or(0, |asked| asked.line)
                        )
                        .into();
                    }
                }
                Binding::Unbound { reason } => {
                    rendered["bound"] = false.into();
                    rendered["unbound"] = serde_json::json!(reason);
                    rendered["says"] = reason.to_string().into();
                }
            }
            rendered
        })
        .collect()
}

/// what a debug script did, step by step
///
/// the whole transcript, because the transcript **is** the answer — an agent
/// given only where a script ended cannot tell why, and will guess. the
/// structure is the core type's own serde and the sentence beside it is the
/// core type's own `Display`, so there is one wording of what happened rather
/// than one per front end
pub fn transcript(ran: &Transcript) -> serde_json::Value {
    let mut rendered = serde_json::json!({
        "at_most": ran.at_most,
        "bytes": ran.bytes,
        "records": ran.records,
        "outcome": ran.outcome,
        "says": ran.outcome.to_string(),
        "partial": ran.partial(),
    });
    if !ran.rebound.is_empty() {
        // loading a file changes what a breakpoint resolves to, and it happened
        // while the script was running the program
        rendered["rebound"] = serde_json::json!(ran.rebound);
    }
    rendered
}

/// one logpoint record
pub fn logged(record: &LogRecord) -> serde_json::Value {
    serde_json::json!({
        "breakpoint": record.breakpoint,
        "file": record.file,
        "line": record.line,
        "thread": record.thread,
        "hit": record.hit,
        "message": record.message,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bpd_core::{Evaluation, Mode, Site, Unbound};

    fn frame(depth: u32) -> bpd_core::Frame {
        bpd_core::Frame {
            id: bpd_core::FrameId { stop: 1, depth },
            file: "/tmp/app.py".to_string(),
            line: 4,
            function: "work".to_string(),
            first_line: 1,
        }
    }

    #[test]
    fn a_stack_that_was_cut_says_how_much_of_it_is_missing() {
        let whole = stack(&Stack {
            frames: vec![frame(0)],
            depth: 1,
            mode: Mode::NonStop,
        });
        assert!(
            whole.get("frames_omitted").is_none(),
            "nothing was left out and it said something was: {whole}"
        );

        let cut = stack(&Stack {
            frames: vec![frame(0)],
            depth: 9,
            mode: Mode::NonStop,
        });
        assert_eq!(cut["frames_omitted"]["count"], 8);
        assert!(
            cut["frames_omitted"]["says"]
                .as_str()
                .expect("a cut says why")
                .contains("`top`"),
            "an elision has to say how to ask for the rest, and said {cut}"
        );
    }

    #[test]
    fn a_breakpoint_that_moved_says_so_and_one_that_did_not_bind_says_why() {
        let requested = vec![
            SourceBreakpoint::at(1, "/tmp/app.py", 7),
            SourceBreakpoint::at(2, "/tmp/later.py", 3),
        ];
        let rendered = breakpoints(
            &[
                Resolved {
                    id: 1,
                    binding: Binding::Bound {
                        line: 9,
                        sites: vec![Site {
                            qualname: "main".to_string(),
                            first_line: 1,
                            offset: 12,
                        }],
                        evaluation: Evaluation::Always,
                    },
                },
                Resolved {
                    id: 2,
                    binding: Binding::Unbound {
                        reason: Unbound::NotLoaded {
                            file: "/tmp/later.py".into(),
                        },
                    },
                },
            ],
            &requested,
        );

        assert_eq!(rendered[0]["bound"], true);
        assert_eq!(rendered[0]["line"], 9);
        assert!(
            rendered[0]["moved"]
                .as_str()
                .expect("it moved")
                .contains("line 9"),
            "said {}",
            rendered[0]
        );

        // never "set". a breakpoint with nothing behind it is reported as
        // having nothing behind it, with the reason
        assert_eq!(rendered[1]["bound"], false);
        assert!(
            rendered[1]["says"]
                .as_str()
                .expect("an unbound breakpoint says why")
                .contains("imported later"),
            "said {}",
            rendered[1]
        );
    }
}
