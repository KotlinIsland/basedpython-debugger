//! what bpd's own sentences look like by the time somebody reads them
//!
//! a long message is written across several source lines with `\` at the end of
//! each, which rust joins into one space-separated sentence. that is the only
//! way a wrapped message stays one sentence, and it is silently easy to lose:
//! generate the same code with a tool whose own strings continue the same way,
//! and the continuation is resolved twice — once by the generator, which keeps
//! the indentation, and never by rust
//!
//! what that leaves is a sentence with thirty spaces in the middle of it, in
//! text an editor puts in front of a person and an agent reads as an answer.
//! nothing catches it: it compiles, the words are right, and no test that
//! matches on a phrase notices the gap between two of them
//!
//! so this reads the source rather than any behaviour, which is unusual here
//! and is the only thing that can see it — `crates/bpd_dap/tests/vscode.rs`
//! reads the extension manifest off disk for the same kind of reason

use std::path::{Path, PathBuf};

/// the workspace's crates, from this test's own location
fn crates() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("a crate lives in the crates directory")
        .to_path_buf()
}

/// every `.rs` file under a crate's `src`
fn sources(within: &Path, into: &mut Vec<PathBuf>) {
    let listing = std::fs::read_dir(within).expect("the workspace is readable");
    for entry in listing {
        let path = entry.expect("a directory entry is readable").path();
        if path.is_dir() {
            sources(&path, into);
        } else if path.extension().is_some_and(|kind| kind == "rs") {
            into.push(path);
        }
    }
}

/// where a run of three or more spaces sits **between two words**
///
/// between two words is the whole rule, and each half of it excludes something
/// deliberate. a run after `\n` is the indentation of a python fixture, and this
/// workspace is full of them. a run before a digit or a brace is a column —
/// `entries      1` in the cache report, a header's padding — lined up on
/// purpose. what is left is prose, where nobody has ever meant to leave a gap
fn gaps(line: &str) -> Option<usize> {
    let characters: Vec<char> = line.chars().collect();
    let mut at = 0;
    while at < characters.len() {
        if characters[at] != ' ' {
            at += 1;
            continue;
        }
        let start = at;
        while at < characters.len() && characters[at] == ' ' {
            at += 1;
        }
        if at - start < 3 || start == 0 || at >= characters.len() {
            continue;
        }

        // the end of a word, and not the `\n` a fixture's next line starts after
        let before = characters[start - 1];
        let ends_a_word = before.is_alphabetic() || before == ',' || before == '.';
        let escaped = start >= 2 && before == 'n' && characters[start - 2] == '\\';

        // and the start of one, rather than a column of numbers or of code
        let after = characters[at];
        let starts_a_word = after.is_alphabetic() || after == '`';

        if ends_a_word && !escaped && starts_a_word {
            return Some(start);
        }
    }
    None
}

/// which part of each line is inside a string literal
///
/// a scanner rather than "does the line have a quote in it", because the
/// sentences worth checking are the long ones and a long one is written across
/// several lines — whose middles carry no quote at all. a guard that skipped
/// those would be blind to exactly the biggest messages bpd has
///
/// raw strings are skipped whole. they hold python fixtures, whose indentation
/// is the program's own and is not prose
fn inside_strings(text: &str) -> Vec<(usize, String)> {
    /// how many `#` a raw string opened with, if this is where one opens
    fn raw_opens(characters: &[char], at: usize) -> Option<usize> {
        if characters.get(at) != Some(&'r') {
            return None;
        }
        let hashes = characters[at + 1..]
            .iter()
            .take_while(|one| **one == '#')
            .count();
        (characters.get(at + 1 + hashes) == Some(&'"')).then_some(hashes)
    }

    /// whether a raw string with this many `#` ends here
    fn raw_closes(characters: &[char], at: usize, hashes: usize) -> bool {
        characters.get(at) == Some(&'"')
            && characters[at + 1..]
                .iter()
                .take(hashes)
                .filter(|one| **one == '#')
                .count()
                == hashes
    }

    /// what is open: a plain literal, or a raw one with this many `#`
    enum Open {
        Plain,
        Raw(usize),
    }

    let mut found = Vec::new();
    let mut open: Option<Open> = None;

    for (number, line) in text.lines().enumerate() {
        // everything is done in **char** positions. a byte index into a line
        // holding an em dash lands inside it, which is a panic rather than a
        // wrong answer, and every message in this workspace has one
        let characters: Vec<char> = line.chars().collect();
        let mut at = 0;
        let mut content = String::new();
        let mut from = 0;

        while at < characters.len() {
            match open {
                Some(Open::Raw(hashes)) => {
                    if raw_closes(&characters, at, hashes) {
                        at += 1 + hashes;
                        open = None;
                    } else {
                        at += 1;
                    }
                }
                Some(Open::Plain) => {
                    if characters[at] == '\\' {
                        at += 2;
                    } else if characters[at] == '"' {
                        content.extend(&characters[from..at]);
                        open = None;
                        at += 1;
                    } else {
                        at += 1;
                    }
                }
                None => {
                    if characters[at] == '/' && characters.get(at + 1) == Some(&'/') {
                        break;
                    }
                    if let Some(hashes) = raw_opens(&characters, at) {
                        open = Some(Open::Raw(hashes));
                        at += 2 + hashes;
                    } else if characters[at] == '"' {
                        open = Some(Open::Plain);
                        at += 1;
                        from = at;
                    } else {
                        at += 1;
                    }
                }
            }
        }
        // a plain literal still open at the end of the line carries on to the
        // next, and the rest of this line is its content
        if matches!(open, Some(Open::Plain)) {
            content.extend(&characters[from.min(characters.len())..]);
        }
        if !content.is_empty() {
            found.push((number + 1, content));
        }
    }
    found
}

#[test]
fn no_sentence_bpd_says_has_a_gap_in_the_middle_of_it() {
    let mut files = Vec::new();
    sources(&crates(), &mut files);
    assert!(
        files.len() > 20,
        "the walk found {} source files, which is not this workspace",
        files.len()
    );

    let mut found = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file).expect("a source file is utf8");
        for (number, content) in inside_strings(&text) {
            if let Some(at) = gaps(&content) {
                found.push(format!(
                    "{}:{number} — a gap of spaces {at} characters into the literal",
                    file.display(),
                ));
            }
        }
    }

    assert!(
        found.is_empty(),
        "these sentences have runs of spaces in the middle of them, which is \
         what a continuation resolved twice leaves behind — they read that way \
         to whoever bpd is talking to:\n{}",
        found.join("\n")
    );
}
