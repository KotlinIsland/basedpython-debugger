"""check the docs tree against the zensical.toml nav

two lists are meant to agree: the pages on disk under `docs/`, and the document
paths in the `zensical.toml` nav. a page missing from the nav is orphaned in the
built site, and a nav entry with no page behind it is a dead link

`zensical build --strict` reports neither, so this is the only guard against the
two drifting apart
"""

from __future__ import annotations

import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).parent.parent
DOCS = ROOT / "docs"


def nav_paths(nav: object) -> list[str]:
    """every document path in the nav, in nav order"""
    if isinstance(nav, str):
        return [nav]
    if isinstance(nav, list):
        return [path for entry in nav for path in nav_paths(entry)]
    if isinstance(nav, dict):
        return [path for value in nav.values() for path in nav_paths(value)]
    return []


def main() -> int:
    config = tomllib.loads((ROOT / "zensical.toml").read_text())
    nav = nav_paths(config["project"]["nav"])

    on_disk = sorted(str(p.relative_to(DOCS)) for p in DOCS.rglob("*.md"))

    problems: list[str] = []

    def report(label: str, items: list[str]) -> None:
        if items:
            problems.append(f"{label}:\n" + "\n".join(f"  - {i}" for i in items))

    report("on disk but not in the zensical.toml nav", sorted(set(on_disk) - set(nav)))
    report(
        "in the zensical.toml nav but missing on disk",
        sorted(p for p in nav if not (DOCS / p).is_file()),
    )
    report(
        "duplicated in the zensical.toml nav",
        sorted({p for p in nav if nav.count(p) > 1}),
    )

    if problems:
        print("\n\n".join(problems), file=sys.stderr)
        print(f"\n{len(problems)} problem(s) — see {Path(__file__).name}", file=sys.stderr)
        return 1

    print(f"{len(on_disk)} pages: the nav and the disk agree")
    return 0


if __name__ == "__main__":
    sys.exit(main())
