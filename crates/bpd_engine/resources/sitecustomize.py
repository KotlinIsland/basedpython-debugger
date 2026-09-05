import os
import sys

agent = os.environ.get("BPD_CHILD_AGENT")
if agent and os.environ.get("BPD_CHILD_ENDPOINT") and os.environ.get("BPD_CHILD_TOKEN"):
    sys.path.insert(0, agent)
    try:
        import bpd_agent
    except Exception as error:
        sys.path.remove(agent)
        sys.stderr.write(
            "bpd: a child of this program is not being debugged: %r\n" % (error,)
        )
    else:
        bpd_agent.child_main()

# this file is first on the path so that it runs, and the program may have a
# sitecustomize of its own further along. that one runs here, as it would have:
# the directory holding this file is taken off the path for one import, and
# whatever the interpreter finds under the name then is what it would have found
# without bpd. none found leaves this module under the name it was imported as
_here = os.path.dirname(os.path.abspath(__file__))
_mine = sys.modules["sitecustomize"]
_was = list(sys.path)
sys.path[:] = [
    entry for entry in sys.path if os.path.abspath(entry or os.curdir) != _here
]
del sys.modules["sitecustomize"]
try:
    import sitecustomize
except ImportError as error:
    if error.name != "sitecustomize":
        raise
    sys.modules["sitecustomize"] = _mine
finally:
    sys.path[:] = _was
