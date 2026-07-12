#!/usr/bin/env python3
"""汇总 refine 回归对比：mineru(输入) vs refined(本次) vs refined_prev(上次快照)。

在仓库根目录运行：python3 .claude/skills/refine-regression/summarize.py [stem]
只打印量级与指标，具体 diff hunks 由调用方按需再看。
"""

import json
import subprocess
import sys
from pathlib import Path

MINERU = Path("test_data/mineru")
NEW = Path("test_data/refined")
PREV = Path("test_data/refined_prev")


def numstat(a: Path, b: Path) -> str:
    if not a.exists():
        return f"n/a（缺 {a}）"
    if not b.exists():
        return f"n/a（缺 {b}）"
    p = subprocess.run(
        ["git", "diff", "--no-index", "--numstat", str(a), str(b)],
        capture_output=True,
        text=True,
    )
    if not p.stdout.strip():
        return "无差异"
    add, rem, _ = p.stdout.split("\t", 2)
    return f"+{add}/-{rem} 行"


def items_len(d: Path):
    f = d / "content_list.json"
    return len(json.loads(f.read_text())) if f.exists() else None


def report(d: Path):
    f = d / "refine_report.json"
    return json.loads(f.read_text()) if f.exists() else None


def fmt_report(r) -> str:
    if r is None:
        return "（无 refine_report.json）"
    tu = r.get("tokenUsage", {})
    return (
        f"iterations={r.get('iterations')} dismissed={r.get('dismissed')} "
        f"violations={r.get('violations')} failOpen={r.get('failOpen')} "
        f"removedSpans={len(r.get('removedSpans', []))} "
        f"tokens p={tu.get('prompt')} c={tu.get('completion')}"
    )


def main():
    only = sys.argv[1] if len(sys.argv) > 1 else None
    if not NEW.is_dir():
        sys.exit("test_data/refined/ 不存在 — 先跑 just refine-real")
    stems = sorted(
        d.name for d in NEW.iterdir() if d.is_dir() and not d.name.startswith(".")
    )
    if only:
        stems = [s for s in stems if s == only]
    if not stems:
        sys.exit("test_data/refined/ 下没有可对比的文档")

    for stem in stems:
        new, old, src = NEW / stem, PREV / stem, MINERU / stem
        print(f"\n════ {stem} ════")
        prev_note = f"（上次 {items_len(old)}）" if old.exists() else "（无上次快照）"
        print(f"items: 输入 {items_len(src)} → 本次 {items_len(new)} {prev_note}")

        rn = report(new)
        ro = report(old) if old.exists() else None
        print(f"本次 report: {fmt_report(rn)}")
        if old.exists():
            print(f"上次 report: {fmt_report(ro)}")
        if ro is not None and rn is not None and rn.get("opCounts") != ro.get("opCounts"):
            print(f"  opCounts 本次: {json.dumps(rn.get('opCounts'), ensure_ascii=False)}")
            print(f"  opCounts 上次: {json.dumps(ro.get('opCounts'), ensure_ascii=False)}")
        elif rn is not None:
            print(f"  opCounts: {json.dumps(rn.get('opCounts'), ensure_ascii=False)}")

        print(f"full.md       输入→本次: {numstat(src / 'full.md', new / 'full.md')}")
        if old.exists():
            print(f"full.md       上次→本次: {numstat(old / 'full.md', new / 'full.md')}")
            print(
                f"content_list  上次→本次: "
                f"{numstat(old / 'content_list.json', new / 'content_list.json')}"
            )


main()
