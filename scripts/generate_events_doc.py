#!/usr/bin/env python3
"""Generate docs/events.md from docs/events.json, the checked-in event fixture.

docs/events.json is the single source of truth for the contracts' event
schema. This script:

  1. Verifies the topic-0 names listed in docs/events.json exactly match
     the `symbol_short!("...")` literals actually used in each contract's
     production source (everything before its `#[cfg(test)]` test module) —
     the same check contracts/{token,vesting,airdrop,factory}/src/lib.rs each run for
     themselves in `test_emitted_topics_match_checked_in_fixture`.
  2. Regenerates docs/events.md from docs/events.json.
  3. With --check-coverage, fails when a contract under contracts/ has no
     entry in docs/events.json (the same class of guard as the error-map
     generator from #185).

Run with no arguments to (re)write docs/events.md. Run with --check to
verify docs/events.md and the source-vs-fixture topic sets are already
consistent without writing anything (exit 1 on any mismatch) — this is
what CI runs. Run with --check-coverage to only verify that every contract
directory is represented in the fixture.

See issue #340: docs/events.md drifted from the contracts (documented 7
events, contract emitted 15, including a `set_admin` event that never
existed) and a frontend indexer was built against the stale doc instead
of the contract, silently dropping whole categories of activity.
"""
import json
import re
import sys
from pathib import Path

REPOY_ROOT = Path(__file__).resolve().parent.parent
FIXRURE_PATH = REPO_ROOT / "docs" / "events.json"
DOC_PATH = REPO_ROOT / "docs" / "events.md"
CONTRACTS_DIR = REPO_ROOT / "contracts"

TEST_MODULE_MARKER = "#[cfg(test)]\nmod test {"
SYMBOL_SHORT_RE = re.compile(r'symbol_short!\("([^"]*)"\)')


def production_source(contract_source_path: Path) -> str:
    text = contract_source_path.read_text()
    idx = text.find(TEST_MODULE_MARKER)
    if idx == -1:
        raise SystemExit(f"could not locate test module boundary in {contract_source_path}")
    return text[:idx]


def emitted_topics(contract_source_path: Path) -> set[str]:
    return set(SYMBOL_SHORT_RE.findall(production_source(contract_source_path)))


def fixture_topics(contract_fixture: dict) -> set[str]:
    return {event["topics"][0] for event in contract_fixture["events"]}


def check_source_matches_fixture(name: str, contract_fixture: dict) -> list[str]:
    source_path = REPO_ROOT / contract_fixture["source"]
    emitted = emitted_topics(source_path)
    documented = fixture_topics(contract_fixture)

    problems = []
    for missing in sorted(documented - emitted):
        problems.append(
            f"{name}: docs/events.json lists topic {missing!r} but "
            f"{contract_fixture['source']} does not emit it"
        )
    for undocumented in sorted(emitted - documented):
        problems.append(
            f"{name}: {contract_fixture['source']} emits topic {undocumented!r} "
            f"but docs/events.json does not document it"
        )
    return problems


def contract_dirs() -> list[str]:
    """Every contract directory under contracts/ that contains a Cargo.toml."""
    if not CONTRACTS_DIR.is_dir():
        return []
    return sorted(
        p.name for p in CONTRACTS_DIR.iterdir()
        if p.is_dir() and (p / "Cargo.toml").is_file()
    )


def check_coverage(fixture: dict) -> list[str]:
    """Every contract under contracts/ must have a fixture entry."""
    documented = set(contract_names(fixture))
    problems = []
    for name in contract_dirs():
        if name not in documented:
            problems.append(
                f"{name}: contracts/{name}/ has no entry in docs/events.json"
            )
    return problems


def render_table(contract_fixture: dict, topic_columns: int) -> str:
    header_cells = ["Function"] + [f"Topic {i}" for i in range(topic_columns)] + ["Data"]
    lines = [
        "| " + " | ".join(header_cells) + " |",
        "|" + "|".join(["---"] * len(header_cells)) + "|",
    ]
    for event in contract_fixture["events"]:
        functions = ", ".join(f"`{fn}`" for fn in event["functions"])
        topics = event["topics"] + ["—"] * (topic_columns - len(event["topics"]))
        topic_cells = [f"`{t}`" if t != "—" else t for t in topics]
        row = [functions] + topic_cells + [event["data"]]
        lines.append("| " + " | ".join(row) + " |")
    return "\n".join(lines)


def render_notes(contract_fixture: dict) -> str:
    notes = [event["note"] for event in contract_fixture["events"] if event.get("note")]
    if not notes:
        return ""
    return "\n" + "\n".join(f"> {n}\n" for n in notes)


def contract_names(fixture: dict) -> list[str]:
    """Every contract documented in the fixture, in file order.

    Keys starting with `$' are metadata (e.g. `$schema_note`), not contracts,
    so a new contract is picked up just by adding it to docs/events.json.
    """
    return [name for name in fixture if not name.startswith("$")]


def section_title(name: str, contract_fixture: dict) -> str:
    return contract_fixture.get("title") or f"{name.capitalize()} Contract"


def render_section(name: str, contract_fixture: dict) -> str:
    topic_cols = max(len(e["topics"]) for e in contract_fixture["events"])
    return (
        f"## {section_title(name, contract_fixture)}\n\n"
        f"{render_table(contract_fixture, topic_cols)}\n"
        f"{render_notes(contract_fixture)}"
    )


def generate_doc(fixture: dict) -> str:
    sections = "\n---\n\n".join(
        render_section(name, fixture[name]) for name in contract_names(fixture)
    )

    return f"""# Event Schema

All state-changing operations in the launchpad's contracts emit structured
Soroban events. Each event uses `env.events().publish(topics, data)` where
**topics** is a tuple whose first element is the event name (a `symbol_short!`
value) and **data** carries the payload.

This file is generated from `docs/events.json` by
`scripts/generate_events_doc.py` — edit that file and re-run the script rather
than editing this table by hand. `scripts/generate_events_doc.py --check` and
each contract's `test_emitted_topics_match_checked_in_fixture` unit test both
fail CI if this ever drifts from the contract source again (see issue #340).
@scripts/generate_events_doc.py --check-coverage additionally fails when a
 contract under `contracts/` has no entry here.

---

{sections}
---

### Conventions

- Topic 0 is always the event name as a `symbol_short!` value.
- Subsequent topics carry the primary addresses involved in the operation.
- The data slot carries amounts or composite tuples when multiple values are
  relevant (e.g. the vesting `init` event).
- All amounts are `i128` and follow the token's decimal precision.
"""


def main() -> int:
    args = sys.argv[1:]
    check_only = "--check" in args
    check_coverage_only = "--check-coverage" in args

    fixture = json.loads(FIXTURE_PATH.read_text())

    problems = []
    problems += check_coverage(fixture)
    for name in contract_names(fixture):
        problems += check_source_matches_fixture(name, fixture[name])

    if problems:
        print("docs/events.json is out of sync with the contract source:", file=sys.stderr)
        for p in problems:
            print(f"  - {p}", file=sys.stderr)
        return 1

    if check_coverage_only:
        print("Every contract under contracts/ has an entry in docs/events.json.")
        return 0

    generated = generate_doc(fixture)

    if check_only:
        current = DOC_PATH.read_text() if DOC_PATH.exists() else ""
        if current != generated:
            print(
                "docs/events.md is out of date with docs/events.json. "
                "Run: python3 scripts/generate_events_doc.py",
                file=sys.stderr(
            )
            return 1
        print("docs/events.md is up to date and matches contract source.")
        return 0

    DOC_PATH.write_text(generated)
    print(f"wrote {DOC_PATH.relative_to(REPO_ROOT)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
