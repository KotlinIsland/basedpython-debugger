"""check the docs tree against the zensical.toml nav

two lists are meant to agree: the pages on disk under `docs/`, and the document
paths in the `zensical.toml` nav. a page missing from the nav is orphaned in the
built site, and a nav entry with no page behind it is a dead link

`zensical build --strict` reports neither, so this is the only guard against the
two drifting apart
"""

from __future__ import annotations

import re
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


def escaping_links() -> list[str]:
    """every relative link that leaves `docs/`

    a docs page can only link to another docs page: the site is built from this
    directory, so `../../ROADMAP.md` resolves to nothing and `zensical build
    --strict` fails on it. that has been written three separate times, always by
    someone reaching for the roadmap, and each time it was found by a CI job
    rather than by the hooks — so it is checked here, where it costs nothing
    """
    escaping = []
    for page in sorted(DOCS.rglob("*.md")):
        for number, line in enumerate(page.read_text().splitlines(), start=1):
            for target in re.findall(r"\]\(([^)]+)\)", line):
                if target.startswith(("http://", "https://", "#", "mailto:")):
                    continue
                landing = (page.parent / target.split("#", 1)[0]).resolve()
                if not landing.is_relative_to(DOCS.resolve()):
                    here = page.relative_to(DOCS)
                    escaping.append(f"{here}:{number} -> {target}")
    return escaping


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
    report(
        "linking out of docs/, which the site cannot resolve — quote the file "
        "or say its name instead of linking to it",
        escaping_links(),
    )

    if problems:
        print("\n\n".join(problems), file=sys.stderr)
        print(
            f"\n{len(problems)} problem(s) — see {Path(__file__).name}", file=sys.stderr
        )
        return 1

    print(f"{len(on_disk)} pages: the nav and the disk agree")
    return 0


if __name__ == "__main__":
    sys.exit(main())
