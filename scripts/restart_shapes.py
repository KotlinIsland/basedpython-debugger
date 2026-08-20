"""measure the two structural claims restart frame's analysis rests on

both are quoted in `crates/bpd_agent/src/bytecode.rs` and in
`docs/development/jumps.md`, and neither was re-runnable until this existed. a
number in a doc comment that nobody can re-derive is the same kind of thing this
feature spent seven review rounds removing, so it is a script rather than a
sentence

    uv run --no-project --python 3.13 python scripts/restart_shapes.py
    uv run --no-project --python 3.14 python scripts/restart_shapes.py

it reads the interpreter it is run by, over that interpreter's own stdlib, and
prints what it counted. the two claims:

**the cost of the strict exit rule.** a line is offered as an exit only when the
walk from *every* one of its `co_lines` range starts reaches a return having run
nothing. the cheaper rule — walk from the lowest start only — is unsound, because
cpython picks the destination by stack depth rather than by offset. what the
strict rule costs is the lines the cheap one accepts and it does not

**what lies past the `to` bound.** a caller's span ends at the end of the
contiguous run holding the call. past that is normally a new line, which is where
the rewind is made from; this counts the permitted call sites where it is instead
a range carrying no line at all, and prints the **whole** run each time — the
conclusion is that none of them runs anything of the program, and an opcode name
on its own does not establish that
"""

from __future__ import annotations

import dis
import pathlib
import sys
import sysconfig

# kept in step with `EXITING` in crates/bpd_agent/src/bytecode.rs
EXITING = {
    "RETURN_VALUE",
    "RETURN_CONST",
    "LOAD_CONST",
    "LOAD_SMALL_INT",
    "LOAD_FAST",
    "LOAD_FAST_BORROW",
    "LOAD_FAST_CHECK",
    "LOAD_DEREF",
    "LOAD_GLOBAL",
    "NOP",
    "NOT_TAKEN",
    "CACHE",
    "EXTENDED_ARG",
}
RETURNING = {"RETURN_VALUE", "RETURN_CONST"}

# kept in step with `BESIDE_THE_CALL`
BESIDE_THE_CALL = {
    "LOAD_FAST",
    "LOAD_FAST_BORROW",
    "LOAD_FAST_CHECK",
    "LOAD_FAST_LOAD_FAST",
    "LOAD_FAST_BORROW_LOAD_FAST_BORROW",
    "LOAD_CONST",
    "LOAD_SMALL_INT",
    "LOAD_GLOBAL",
    "LOAD_DEREF",
    "LOAD_NAME",
    "PUSH_NULL",
    "COPY",
    "SWAP",
    "BUILD_TUPLE",
    "BUILD_LIST",
    "POP_TOP",
    "STORE_FAST",
    "STORE_FAST_STORE_FAST",
    "STORE_FAST_LOAD_FAST",
    "STORE_DEREF",
    "STORE_GLOBAL",
    "STORE_NAME",
    "NOP",
    "NOT_TAKEN",
    "RESUME",
    "CACHE",
    "EXTENDED_ARG",
}
CALLING = {"CALL", "CALL_KW", "CALL_FUNCTION_EX"}

# what the tail can write where the **callee** reads it. `STORE_NAME` is the
# same hazard when the frame's namespace is the global namespace, which is what
# the module-body count below is for
SHARED_WITH_THE_CALLEE = {"STORE_GLOBAL", "STORE_DEREF"}


def walks_clean(instructions: list, frm: int) -> bool:
    """the agent's `walk_to_a_return`, with no frame to ask about namespaces"""
    for one in instructions:
        if one.offset < frm:
            continue
        if one.opname in RETURNING:
            return True
        if one.opname not in EXITING:
            return False
    return False


def code_objects(top):
    """every code object under `top`, and whether each **is** `top`

    a module body's namespace is the global namespace, so its `STORE_NAME`
    reaches the callee where a class body's does not — which makes "is this the
    module body" a question this sweep has to carry
    """
    stack = [(top, True)]
    while stack:
        code, is_module = stack.pop()
        yield code, is_module
        for const in code.co_consts:
            if hasattr(const, "co_code"):
                stack.append((const, False))


def sources() -> list[pathlib.Path]:
    root = pathlib.Path(sysconfig.get_paths()["stdlib"])
    return sorted(path for path in root.rglob("*.py") if "test" not in path.parts)


def main() -> int:
    lost_lines = 0
    lost_objects = 0
    with_an_exit = 0
    examples: list[str] = []

    permitted = 0
    # what the tail writes where the **callee** can read it, which is what
    # `TailWritesSharedState` refuses. quoted in `bpd_core::jump` and in
    # `docs/development/jumps.md`, and not derivable from anything else here
    shared = 0
    in_a_module = 0
    module_refused = 0
    past_the_bound = 0
    openers: dict[str, int] = {}

    files = sources()
    for path in files:
        try:
            # `dont_inherit` because this module has `from __future__ import
            # annotations` and `compile` inherits the caller's future flags. on
            # 3.14 that switches PEP 649 lazy annotations back off, so every
            # class body loses its `MAKE_CELL __classdict__` prologue and every
            # offset after it shifts — the script would be measuring bytecode
            # the interpreter never runs, which is the one thing it exists not
            # to do. it cost 11 permitted call sites on 3.14 and a figure in
            # four doc comments
            top = compile(
                path.read_text(errors="replace"),
                str(path),
                "exec",
                dont_inherit=True,
            )
        except (SyntaxError, ValueError, UnicodeDecodeError):
            continue
        for code, is_module in code_objects(top):
            instructions = list(dis.get_instructions(code))
            spans = list(code.co_lines())
            lined = [(o, e, l) for o, e, l in spans if l is not None and l >= 1]
            if not lined:
                continue

            starts: dict[int, list[int]] = {}
            for offset, _end, line in lined:
                starts.setdefault(line, []).append(offset)

            lowest = {
                line
                for line, offsets in starts.items()
                if walks_clean(instructions, min(offsets))
            }
            every = {
                line
                for line, offsets in starts.items()
                if all(walks_clean(instructions, offset) for offset in offsets)
            }
            if lowest:
                with_an_exit += 1
            lost_lines += len(lowest - every)
            if lowest and not every:
                lost_objects += 1
                if len(examples) < 8:
                    examples.append(
                        f"{code.co_qualname} in {path.name} "
                        f"line(s) {sorted(lowest - every)}"
                    )

            at = {one.offset: one.opname for one in instructions}
            for call in (one for one in instructions if one.opname in CALLING):
                holding = next(
                    ((o, e, l) for o, e, l in lined if o <= call.offset < e), None
                )
                if holding is None:
                    continue
                _start, end, line = holding
                frm = min(starts[line])
                span = [one for one in instructions if frm <= one.offset < end]
                if any(one.opname in RETURNING for one in span):
                    continue
                if sum(1 for one in span if one.opname in CALLING) != 1:
                    continue
                # the agent refuses `f(*args)` outright — `CALL_FUNCTION_EX`
                # unpacks by iterating, so the second call would not be the call
                # that was restarted. counting those as permitted made this
                # script print a number the agent's own comments could not be
                # checked against, which is the one thing it exists to prevent
                if call.opname == "CALL_FUNCTION_EX":
                    continue
                if any(
                    one.opname not in BESIDE_THE_CALL and one.offset != call.offset
                    for one in span
                ):
                    continue
                permitted += 1
                after = [one for one in span if one.offset > call.offset]
                if any(one.opname in SHARED_WITH_THE_CALLEE for one in after):
                    shared += 1
                if is_module:
                    in_a_module += 1
                    if any(one.opname == "STORE_NAME" for one in after):
                        module_refused += 1
                after = next((s for s in spans if s[0] == end), None)
                if after is not None and after[2] is None:
                    past_the_bound += 1
                    # the whole run, not just its first opcode. printing only the
                    # opener left a reader unable to tell whether an
                    # `EXTENDED_ARG` prefixed a jump or something that runs, and
                    # the conclusion turns on exactly that
                    run = [
                        one.opname
                        for one in instructions
                        if end <= one.offset < after[1]
                    ]
                    key = " ".join(run) if run else "(empty)"
                    openers[key] = openers.get(key, 0) + 1

    build = "" if sys._is_gil_enabled() else "t"
    print(
        f"python {'.'.join(map(str, sys.version_info[:3]))}{build} — "
        f"{len(files)} stdlib files"
    )
    print()
    print("the cost of requiring every range start to walk clean")
    print(f"  code objects with at least one exit line   {with_an_exit}")
    print(f"  lines the lowest-start rule accepts and this one does not   {lost_lines}")
    print(f"  code objects that lose their last candidate   {lost_objects}")
    for example in examples:
        print(f"    {example}")
    print()
    print("what lies past the `to` bound, over the call sites the rule permits")
    print(f"  call sites permitted   {permitted}")
    print(f"  of those, the tail writes a global or a cell   {shared}")
    print()
    print("what the module-body rule costs, against its own denominator")
    print(f"  permitted call sites in a module body   {in_a_module}")
    print(f"  of those, refused for a tail store   {module_refused}")
    print(f"  of those, still restart   {in_a_module - module_refused}")
    print(f"  of those, the run past `to` carries no line   {past_the_bound}")
    for run, count in sorted(openers.items(), key=lambda kv: -kv[1]):
        print(f"    {count} x  {run}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
