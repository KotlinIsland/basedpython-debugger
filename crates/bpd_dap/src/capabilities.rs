//! what this adapter tells a client it can do
//!
//! a capability reported here and not implemented is a placeholder with a wire
//! format: the client hides the feature behind it, or offers it and gets an
//! error at the moment the user needs it. so this list is short, and the
//! reasons the obvious absences are absent are written down beside it rather
//! than left for someone to rediscover
//!
//! ## what is deliberately not advertised, and why
//!
//! - **`supportsHitConditionalBreakpoints`** — DAP carries a hit condition as a
//!   free-form *string*, and what it means is a per-client convention:
//!   `>5`, `=5`, `%5` and a bare `5` are read differently by different
//!   adapters. [`bpd_core::HitCondition`] is deliberately not that string, and
//!   an adapter that guessed which convention a client meant would be a
//!   debugger that stops on the wrong pass. the capability exists in the core
//!   and is reachable from a front end whose request can say which it means
//! - **`supportsEvaluateForHovers`** — an evaluation runs the program's own
//!   code. running it because a mouse passed over an identifier is the debugger
//!   changing the program by accident, so evaluation stays where the user asked
//!   for it
//! - **`supportsSetExpression`**, **`supportsFunctionBreakpoints`**,
//!   **`supportsDataBreakpoints`**, **`supportsStepBack`**,
//!   **`supportsRestartRequest`** — no capability behind them exists yet
//! - **`supportsDelayedStackTraceLoading`** — [`bpd_core::Request::Stack`]
//!   bounds a walk by how many frames from the top, and has no way to start
//!   part way down. a client that paged from the middle would be answered from
//!   a walk that started at the top anyway, which is a claim about cost that is
//!   not true
//! - **`supportsTerminateRequest`** — **not advertised, and serviced anyway.**
//!   the adapter answers `terminate` exactly as it answers `disconnect`, because
//!   a client that sends one has said the same thing either way and refusing it
//!   would leave a session open that the client believes it has ended. what is
//!   withheld is the *promise*: there is no graceful end. `disconnect` ends the
//!   debuggee, and advertising a second request under a name that promises more
//!   would be a promise nothing keeps

/// the `initialize` response body
pub fn capabilities() -> serde_json::Value {
    serde_json::json!({
        // the client tells bpd when it has finished sending breakpoints, which
        // is what lets the program be held at entry until they are all bound
        "supportsConfigurationDoneRequest": true,

        // `SourceBreakpoint::condition`, compiled in the debuggee when the
        // breakpoint is set — a condition that does not compile makes the
        // breakpoint unbound rather than firing on every pass
        "supportsConditionalBreakpoints": true,

        // `SourceBreakpoint::log`, formatted in the debuggee. `{...}` is a
        // python expression and `{{` is a literal brace, which is DAP's own
        // convention and bpd's
        "supportsLogPoints": true,

        // `Request::SetVariable`, which refuses a name the frame's code object
        // does not have rather than writing one the program can never read
        "supportsSetVariable": true,

        // `Request::Evaluate`, in a named frame
        "supportsEvaluateForHovers": false,

        // `Request::SetNextStatement`. a target is minted for the location the
        // client asks about, and only when it is the file the held thread is
        // executing: a line number means nothing without its file, and cpython
        // would accept the same number against another one
        "supportsGotoTargetsRequest": true,

        // `Request::RestartFrame`, which re-enters the frame the thread is
        // executing with what its parameters hold **now**. DAP's own wording
        // for this request implies discarding the frames above the one named,
        // and there is no mechanism for that — so a frame that is not the
        // executing one is refused with the reason rather than approximated
        "supportsRestartFrame": true,

        // a stop holds **one thread** and the rest of the program keeps
        // running. this is the capability that says so: without it a client is
        // entitled to believe every stop is a whole-program stop, and every
        // `stopped` event bpd sends would be telling it something untrue
        "supportsSingleThreadExecutionRequests": true,

        // `StopReason::Raised` and `StopReason::Uncaught` carry the exception
        // and its traceback, which is exactly what this request asks for
        "supportsExceptionInfoRequest": true,

        "exceptionBreakpointFilters": [
            {
                "filter": "raised",
                "label": "raised exceptions",
                "description":
                    "stop where an exception is raised, whether or not anything \
                     catches it. reported once, in the frame that raised it",
                "default": false,
            },
            {
                "filter": "uncaught",
                "label": "uncaught exceptions",
                "description":
                    "stop where an exception leaves the outermost frame. only \
                     knowable as it unwinds, so the stop is there rather than \
                     where it was raised",
                "default": true,
            },
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_is_advertised_as_true_unless_a_request_implements_it() {
        let advertised = capabilities();
        let table = advertised
            .as_object()
            .expect("the capabilities are an object");

        let claimed: Vec<&str> = table
            .iter()
            .filter(|(_, value)| value.as_bool() == Some(true))
            .map(|(name, _)| name.as_str())
            .collect();

        // the list is written out rather than counted, so adding a capability
        // to the json without implementing the request behind it fails here
        assert_eq!(
            claimed,
            vec![
                "supportsConditionalBreakpoints",
                "supportsConfigurationDoneRequest",
                "supportsExceptionInfoRequest",
                "supportsGotoTargetsRequest",
                "supportsLogPoints",
                "supportsRestartFrame",
                "supportsSetVariable",
                "supportsSingleThreadExecutionRequests",
            ]
        );
    }

    #[test]
    fn the_thread_model_is_declared_rather_than_left_to_be_assumed() {
        // a stop holds one thread and the others keep running. a client that
        // did not know that would render a whole-program stop, and every read
        // it took would be labelled with a mode it was not taken in
        assert_eq!(
            capabilities()["supportsSingleThreadExecutionRequests"],
            true
        );
    }
}
