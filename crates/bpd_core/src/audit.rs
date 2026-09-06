//! every audit event the agent's native hook recognises, by purpose
//!
//! the hook is one function called for every audit event the process raises,
//! and the first thing it does is compare the event's name against a short
//! list. the list used to be the process-making events alone, and it lived in
//! [`crate::spawn`] because two things need the same answer: the agent that
//! compares, and the parity suite that has to know a fixture reached the hook.
//! a second purpose arrived with the trace ring of basedpython-ui, whose
//! runtime announces every record it appends as
//! [`crate::recompose::TRACE_EVENT`]
//!
//! so the lists stay per purpose — [`crate::spawn::making_a_process`] is still
//! what a child is recognised by, and nothing about it changed — and this is
//! the one place that says what the hook watches in total. a front end reading
//! either list reads the same names the hook compares against

use std::ffi::CStr;

use crate::recompose::TRACE_EVENT;
use crate::spawn::making_a_process;

/// every audit event the hook recognises on an interpreter of this version
///
/// the process-making events of that release, and the trace event beside them.
/// the trace event does not change with the release: it is the runtime's and
/// not the interpreter's
#[must_use]
pub fn watched(major: u8, minor: u8) -> Vec<&'static CStr> {
    let mut all = making_a_process(major, minor).to_vec();
    all.push(TRACE_EVENT);
    all
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_trace_event_is_watched_beside_the_process_making_ones_and_is_none_of_them() {
        // the two purposes share one hook and one comparison, and a name on
        // both lists would be an event read as a child and as a record at once
        for (major, minor) in [(3, 13), (3, 14), (3, 15)] {
            let all = watched(major, minor);
            assert!(all.contains(&TRACE_EVENT));
            assert!(
                !making_a_process(major, minor).contains(&TRACE_EVENT),
                "the trace event is not a way of making a process"
            );
            assert_eq!(
                all.len(),
                making_a_process(major, minor).len() + 1,
                "the whole list is the process-making events and the trace event"
            );
        }
    }
}
