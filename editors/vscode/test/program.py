"""the program the session in `session.js` debugs

it is here rather than written by the runner so that the line a breakpoint goes
on is a line somebody can read. the breakpoint is placed by searching for the
comment below, so editing this file moves the breakpoint with it instead of
silently aiming it at a different statement

the last statement writes the file the runner looks for afterwards. a session
that was killed rather than resumed never reaches it, so that file is the
program's own word that it ran to the end
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
