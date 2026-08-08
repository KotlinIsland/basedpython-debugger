"""report what a program can see about how it was launched

running a program under a debugger has to be indistinguishable from running it
directly, and these are the values that give it away. this file is run three
ways — as a script, as `-m`, and as `-c` — and every field below differs
between at least two of them

it is also passed to `python -c` verbatim, so it must stay a single file with
no imports of its own beyond the standard library
"""

import json
import sys

main = sys.modules["__main__"]
spec = main.__spec__

sys.stdout.write(
    json.dumps(
        {
            "argv": sys.argv,
            "path0": sys.path[0],
            "name": __name__,
            "file": getattr(main, "__file__", None),
            "package": getattr(main, "__package__", None),
            "spec": None if spec is None else spec.name,
            "executable": sys.executable,
        }
    )
)
