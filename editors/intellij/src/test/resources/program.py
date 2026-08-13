"""the program the session in `BpdSessionTest` debugs

it is a copy of `editors/vscode/test/program.py`, deliberately: the two suites
drive two editors against the same adapter, and a difference between them would
be a difference nobody chose

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
    pathlib.Path(sys.argv[1]).write_text(str(accumulate()), encoding="utf-8")
