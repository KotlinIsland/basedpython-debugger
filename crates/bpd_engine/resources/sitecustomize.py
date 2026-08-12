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
