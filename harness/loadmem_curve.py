#!/usr/bin/env python3
"""loadmem_curve.py RUN_DIR [--every-mb N]: the growth curve of a `loadmem.py` run as a table.

Reads RUN_DIR/phases.jsonl, which has every line of the worker's log in order (the worker logs a
`memory tick` line every RA_JIT_MEMORY_EVERY seconds, between the lines that say which phase it is
in) and RUN_DIR/series.jsonl (the footprint of the whole process tree every sample). Prints, as
markdown, one row each time the worker's live heap passes another N MB (default 1024), and one for
each tick that follows a phase line, with the phase line it follows. Python 3 standard library only.
"""
import json, re, sys

TICK = re.compile(r"memory tick (\d+) at (\d+)s: (\d+) MB footprint, (\d+) MB live, (\d+) MB active, "
                  r"(\d+) MB resident, (\d+) MB mapped, (\d+) MB retained, (\d+) MB metadata(?:, (\d+) functions compiled)?")


def main():
    run = sys.argv[1]
    step = int(sys.argv[sys.argv.index("--every-mb") + 1]) if "--every-mb" in sys.argv else 1024
    lines = [json.loads(l) for l in open(f"{run}/phases.jsonl")]
    worker = [l for l in lines if "/workers/" in l["log"] or "/spares/" in l["log"]]
    print("| worker s | seen at run s | footprint MB | live MB | active MB | resident MB | mapped MB | functions compiled | after the line |")
    print("|---|---|---|---|---|---|---|---|---|")
    last_line, next_live, shown = "", 0, 0
    pending = False
    for l in worker:
        m = TICK.search(l["line"])
        if not m:
            if not l["line"].startswith("memory after"):
                last_line = l["line"][:90]
                pending = True
            continue
        tick, secs, foot, live, active, resident, mapped, retained, _, compiled = (int(it or 0) for it in m.groups())
        if live >= next_live or pending:
            print(f"| {secs} | {l['t']:.0f} | {foot} | {live} | {active} | {resident} | {mapped} | {compiled or '-'} | {last_line} |")
            next_live = max(next_live, (live // step + 1) * step)
            pending = False
            shown += 1
    print(f"\n{shown} rows of {sum(1 for l in worker if TICK.search(l['line']))} ticks.")


if __name__ == "__main__":
    main()
