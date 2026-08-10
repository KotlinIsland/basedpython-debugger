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
loader = main.__dict__.get("__loader__")

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
            # the names the interpreter itself put in `__main__`, which differ
            # by form: only `-m` and a script carry `__file__` and `__cached__`
            "dunders": sorted(
                name
                for name in main.__dict__
                if name.startswith("__") and name.endswith("__")
            ),
            # everything else in `__main__`, which is this file's own names and
            # nothing more. a `__main__` the debugger built out of the module it
            # bootstrapped through would have the agent in here
            "globals": sorted(
                name for name in main.__dict__ if not name.startswith("__")
            ),
            "cached": main.__dict__.get("__cached__"),
            # a class for `-c` (`BuiltinImporter`) and an instance for the other
            # two, so the name is read off whichever it is
            "loader": (
                None
                if loader is None
                else getattr(loader, "__name__", type(loader).__name__)
            ),
            # cpython gives `__main__` the builtins **module**, where an
            # ordinary `exec` would leave its dict
            "builtins": type(main.__dict__.get("__builtins__")).__name__,
            # `-P` and `PYTHONSAFEPATH` turn off the prepending that decides
            # `sys.path[0]`, so a test of it has to be able to say it was on
            "safe_path": bool(sys.flags.safe_path),
        }
    )
)
