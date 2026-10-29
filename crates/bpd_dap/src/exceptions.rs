//! what a client has asked for of exceptions, through the two requests that ask
//!
//! DAP's own `setExceptionBreakpoints` carries the filters and
//! `exceptionOptions`, and an IDE's DAP layer owns it: the intellij platform
//! sends it itself, again on every change, with no field for a class's library
//! choice and none for anything a plugin wants to add. so a rule per class has
//! a second way in that no platform sends — `exceptionClasses` in the launch
//! configuration, and `bpd/setExceptionClasses` after it — and each request
//! replaces **only its own half**. a change of the filters never touches the
//! rules from the other request, and a change of the rules never touches the
//! filters
//!
//! the agent holds one table, so the two halves are sent as one: the rules from
//! `exceptionClasses` first, then each `exceptionOptions` class no rule names.
//! where both name a class, the rule decides, because it is the one that can
//! say what it does about a library's raise. everything else about how a rule
//! decides is the agent's — see [`bpd_core::ExceptionClass`]

use bpd_core::{ArmedClass, ClassFound, ExceptionClass};

/// both halves, as they were last asked for
#[derive(Debug, Clone, Default)]
pub struct Exceptions {
    /// stopping where an exception is raised, from `setExceptionBreakpoints`
    pub raised: bool,
    /// stopping where an exception leaves the program, from
    /// `setExceptionBreakpoints`
    pub uncaught: bool,
    /// the classes `exceptionOptions` named, with what each one's break mode
    /// asks
    ///
    /// their library choice is the session's `justMyCode`, decided when they
    /// are sent: an option has nowhere to say one, and one asked for before the
    /// launch has no configuration yet to take it from
    pub options: Vec<ExceptionClass>,
    /// the rules `exceptionClasses` and `bpd/setExceptionClasses` set
    pub classes: Vec<ExceptionClass>,
    /// whether either request has asked for anything since the agent was last
    /// told, because there was no agent to tell
    pub held: bool,
}

impl Exceptions {
    /// the one table the agent is sent: the rules, then each option no rule
    /// names, an option ignoring library files exactly when `just_my_code` is on
    pub fn merged(&self, just_my_code: bool) -> Vec<ExceptionClass> {
        let mut table = self.classes.clone();
        for option in &self.options {
            if !table.iter().any(|rule| rule.name == option.name) {
                table.push(ExceptionClass {
                    libraries: !just_my_code,
                    ..option.clone()
                });
            }
        }
        table
    }
}

/// what one rule or option is answered with, read back off what the agent holds
///
/// a DAP `Breakpoint`'s three fields, which is what both requests answer each
/// entry with: `verified` when the agent holds exactly this rule **and** its
/// class is one the program has now. a rule whose class the program does not
/// have yet is held all the same and is `pending`, the word a breakpoint in a
/// module not imported yet gets — it decides for its class the moment one is
/// raised. anything that will not decide as asked is `failed`, with why
pub fn verdict(
    asked: &ExceptionClass,
    armed: &[ArmedClass],
    passed_over: &str,
) -> serde_json::Value {
    let Some(held) = armed.iter().find(|held| held.rule.name == asked.name) else {
        return failed(&format!(
            "the debuggee does not hold a rule for `{}`",
            asked.name
        ));
    };
    if held.rule != *asked {
        return failed(passed_over);
    }
    let name = &asked.name;
    match &held.found {
        ClassFound::Class => serde_json::json!({ "verified": true }),
        ClassFound::Unresolved { why } => serde_json::json!({
            "verified": false,
            "reason": "pending",
            "message": format!(
                "no class the program has now is `{name}`: {why}. the rule is held, and \
                 decides for a class that gives itself this name when one is raised"
            ),
        }),
        ClassFound::NotAnException { what } => failed(&format!(
            "`{name}` is {what}, so nothing the program raises is one. the rule is held, and \
             decides only for a class that gives itself this name"
        )),
        ClassFound::Renamed { named } => failed(&format!(
            "`{name}` is a class that gives itself the name `{named}`, and a rule decides for \
             a class by the name the class gives itself, so this one never decides for it. \
             name it `{named}`"
        )),
    }
}

/// an entry that will not decide as asked, and why
pub fn failed(message: &str) -> serde_json::Value {
    serde_json::json!({
        "verified": false,
        "reason": "failed",
        "message": message,
    })
}

/// what every entry is answered with while there is no program to hold it
pub fn waiting() -> serde_json::Value {
    serde_json::json!({
        "verified": false,
        "reason": "pending",
        "message": "no program has been started yet, so there is no interpreter to hold \
                    this rule. it is kept exactly as it was asked for and armed before the \
                    program runs its first line",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(name: &str, raised: bool, libraries: bool) -> ExceptionClass {
        ExceptionClass {
            name: name.to_string(),
            raised,
            uncaught: false,
            libraries,
        }
    }

    #[test]
    fn a_rule_decides_in_place_of_an_option_for_the_same_class_and_the_rest_are_both() {
        let exceptions = Exceptions {
            options: vec![rule("KeyError", true, false), rule("OSError", true, false)],
            classes: vec![
                rule("KeyError", false, true),
                rule("ValueError", true, true),
            ],
            ..Exceptions::default()
        };
        assert_eq!(
            exceptions.merged(false),
            [
                rule("KeyError", false, true),
                rule("ValueError", true, true),
                rule("OSError", true, true),
            ],
            "the rules first, then the options no rule names, an option's library \
             choice the session's"
        );
        assert_eq!(exceptions.merged(true)[2], rule("OSError", true, false));
    }
}
