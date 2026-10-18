"""Descriptive rg counts per dependency family (not a selection rule).

  python -I stats.py --gold-dir <dependency dir> --lci-tree <clean lci export> --vite-tree <export> --rg <rg>
                     [--ha-tree <export>]

For each family runs the premise command at the tree root,
  rg --no-config -n -w --hidden -g '!.git' --sort path --max-filesize 1M -- <name> .
and prints a markdown table: all output lines, lockfile lines (the 11 basenames of the prereg, section 3), lockfile
lines among the first 50 output lines, and the 1-based rank of the gold line in the output.
"""
import argparse
import json
import pathlib
import re
import subprocess

LOCKFILES = {
    "package-lock.json", "npm-shrinkwrap.json", "composer.lock", "Cargo.lock", "yarn.lock", "pnpm-lock.yaml",
    "Pipfile.lock", "poetry.lock", "uv.lock", "bun.lock", "bun.lockb",
}
ap = argparse.ArgumentParser()
ap.add_argument("--gold-dir", required=True)
ap.add_argument("--lci-tree", required=True)
ap.add_argument("--vite-tree", required=True)
ap.add_argument("--ha-tree")
ap.add_argument("--rg", required=True)
a = ap.parse_args()
trees = {"lci": a.lci_tree, "vite": a.vite_tree, "home-assistant": a.ha_tree}
print("| Family | All lines | Lockfile lines | Lockfile lines in first 50 | Rank of gold line |")
print("| --- | ---: | ---: | ---: | ---: |")
for path in sorted(pathlib.Path(a.gold_dir).glob("*.json")):
    gold = json.loads(path.read_text(encoding="utf-8"))
    for fam in gold["families"]:
        cmd = [a.rg, "--no-config", "-n", "-w", "--hidden", "-g", "!.git", "--sort", "path", "--max-filesize", "1M",
               "--", fam["key"], "."]
        p = subprocess.run(cmd, cwd=trees[gold["repo"]], stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        assert p.returncode in (0, 1), p.stderr[:200]
        hits = []
        for raw in (p.stdout.split(b"\n")[:-1] if p.stdout else []):
            m = re.match(rb"^\.[/\\](.*?):(\d+):", raw)
            hits.append((m.group(1).decode("utf-8", errors="replace").replace("\\", "/"), int(m.group(2))))
        lock = [h[0].rsplit("/", 1)[-1] in LOCKFILES for h in hits]
        g = fam["assignments"][0]
        rank = [i + 1 for i, h in enumerate(hits) if h == (g["path"], g["line"])]
        assert len(rank) == 1, (fam["id"], rank)
        print(f"| {fam['id']} | {len(hits)} | {sum(lock)} | {sum(lock[:50])} | {rank[0]} |")
