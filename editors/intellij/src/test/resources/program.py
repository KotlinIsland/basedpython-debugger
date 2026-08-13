"""the program the session in `BpdSessionTest` debugs

it is `editors/vscode/test/program.py` with two `print` calls added, and nothing
else. the two suites drive two editors against the same adapter, so a difference
between them would otherwise be a difference nobody chose — this one is chosen:
the intellij suite asserts that what the program writes reaches the IDE console,
and a program that writes nothing cannot say whether it does

`flush=True` on the first of them is not decoration. bpd hands the interpreter a
pipe rather than a terminal, and cpython block-buffers a stdout that is a pipe,
so an unflushed line sits in the interpreter until it exits and is not in the
console while the program is stopped. stderr is line-buffered either way. the
test reads both at the breakpoint, so the program flushes the one that would
otherwise not be there yet rather than the test waiting for a program to end

the breakpoint is placed by searching for the comment below, so editing this
file moves the breakpoint with it instead of silently aiming it at a different
statement

the last statement writes the file the test looks for afterwards. a session that
was killed rather than resumed never reaches it, so that file is the program's
own word that it ran to the end
"""

import pathlib
import sys


def accumulate():
    total = 0
    for n in range(3):
        total += n
    answer = total + 39  # bpd: the session stops here
    return answer


if __name__ == "__main__":
    print("bpd: the program wrote this on stdout", flush=True)
    print("bpd: the program wrote this on stderr", file=sys.stderr)
    pathlib.Path(sys.argv[1]).write_text(str(accumulate()), encoding="utf-8")
