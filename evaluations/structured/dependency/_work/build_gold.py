"""Build the dependency-stratum gold files from frame.json and decisions.json, reading every manifest from the
pinned git blob (never a working tree). Run with python -I. Paths are command-line inputs, never written out.

  python -I build_gold.py --lci-git <worktree> --vite-git <bare repo> --ha-git <bare repo> [--check]

Writes <dependency dir>/<repo>.json for every repository with at least one accepted family (split "pending";
run split.py afterwards). With --check nothing is written; the files on disk must equal what would be written.
The builder refuses a decisions.json that is not a prefix walk of the frame order (no cherry-picking): accepted
and skipped candidates, merged and sorted by frame position, must be the first candidates of the frame, with
accepted ones ending at 8 or at the end of the frame.
"""
import argparse
import hashlib
import json
import pathlib
import subprocess

HERE = pathlib.Path(__file__).resolve().parent
OUT = HERE.parent
REVS = {
    "lci": "579a5bec69309d98c78825654aabe8f03ab64421",
    "vite": "7a89794331526a67bc9b29047b86957c581c128c",
    "home-assistant": "bd92409b6ade6e2873cac603104f23e5d9e7ec61",
}
SEED = 20261018
MAX_FAMILIES = 8

ap = argparse.ArgumentParser()
ap.add_argument("--lci-git", required=True)
ap.add_argument("--vite-git", required=True)
ap.add_argument("--ha-git", required=True)
ap.add_argument("--check", action="store_true")
a = ap.parse_args()
GITS = {
    "lci": ["git", "-C", a.lci_git],
    "vite": ["git", "--git-dir=" + a.vite_git],
    "home-assistant": ["git", "--git-dir=" + a.ha_git],
}
frame = json.loads((HERE / "frame.json").read_text(encoding="utf-8"))
decisions = json.loads((HERE / "decisions.json").read_text(encoding="utf-8"))
assert frame["revisions"] == REVS and frame["seed"] == SEED


def blob(repo, path):
    assert not pathlib.PurePosixPath(path).is_absolute() and "\\" not in path and ".." not in path.split("/")
    return subprocess.check_output(GITS[repo] + ["show", f"{REVS[repo]}:{path}"])


def build(repo):
    order = frame["repos"][repo]["order"]
    names = [o["name"] for o in order]
    acc = decisions["repos"][repo]["accepted"]
    skip = decisions["repos"][repo]["skipped"]
    visited = sorted([x["name"] for x in acc] + [x["name"] for x in skip], key=names.index)
    assert visited == names[: len(visited)], (repo, "decisions are not a prefix walk of the frame order")
    assert len(acc) <= MAX_FAMILIES
    assert len(acc) == MAX_FAMILIES or len(visited) == len(names), (repo, "stopped before 8 accepted")
    for s in skip:
        assert s.get("reason"), s
    families, questions = [], []
    for x in sorted(acc, key=lambda x: names.index(x["name"])):
        entry = order[names.index(x["name"])]
        decls = entry["declarations"]
        assert 1 <= len(decls) <= 3, (x["name"], len(decls))
        first = sorted(d["path"].encode("utf-8") for d in decls)[0].decode("utf-8")
        assert x["path"] == first, (x["name"], "gold manifest is not the first bytewise", first)
        mine = [d for d in decls if d["path"] == x["path"]]
        assert len(mine) == 1, (x["name"], "declared more than once in the chosen manifest")
        d = mine[0]
        assert not d.get("table_form")
        raw = blob(repo, d["path"]).replace(b"\r\n", b"\n")
        lines = raw.decode("utf-8-sig").split("\n")
        line, end = d["line"], d["end_line"]
        assert 1 <= line <= end <= len(lines)
        span_text = "\n".join(lines[line - 1:end])
        assert x["name"] in span_text, (x["name"], span_text)
        is_json = d["path"].endswith(".json")
        if is_json:
            value = span_text.split(":", 1)[1].strip().rstrip(",").strip()
        else:
            value = span_text.split("=", 1)[1].strip()
        fid = f"{repo}.dep.{x['name']}"
        text = x["text"]
        assert x["name"] in text and d["path"] not in text
        families.append({
            "id": fid,
            "stratum": "dependency",
            "split": "pending",
            "format": "json" if is_json else "toml",
            "key": x["name"],
            "assignments": [{
                "path": d["path"], "line": line, "end_line": end,
                "sha256": hashlib.sha256(raw).hexdigest(), "value": value, "kind": "assignment",
            }],
            "readers": [],
            "readers_scope": "none:package-manager",
            "excluded_readers": [],
            "behaviour": f"Declares the {d['section']} version of {x['name']} for the {x['role']}.",
            "docs": [],
        })
        questions.append({"id": fid + ".q1", "family": fid, "type": "setting_location", "text": text,
                          "names_key": True})
    return {"version": 1, "repo": repo, "revision": REVS[repo], "seed": SEED, "families": families,
            "questions": questions}


changed = 0
for repo in REVS:
    if not decisions["repos"][repo]["accepted"]:
        print(f"{repo}: no accepted families, no file")
        continue
    gold = build(repo)
    path = OUT / f"{repo}.json"
    if path.exists() and not a.check:
        # keep splits already assigned by split.py for unchanged families
        old = {f["id"]: f["split"] for f in json.loads(path.read_text(encoding="utf-8"))["families"]}
        for f in gold["families"]:
            f["split"] = old.get(f["id"], f["split"])
    text = json.dumps(gold, indent=2, ensure_ascii=False) + "\n"
    if a.check:
        on_disk = path.read_text(encoding="utf-8") if path.exists() else None
        have = json.loads(on_disk) if on_disk else None
        for f in gold["families"]:
            f["split"] = next((g["split"] for g in (have or {"families": []})["families"] if g["id"] == f["id"]),
                              f["split"])
        same = have == gold
        print(f"{repo}: {'matches' if same else 'DIFFERS'}")
        changed += not same
    else:
        path.write_text(text, encoding="utf-8", newline="\n")
        print(f"{repo}: wrote {len(gold['families'])} families")
raise SystemExit(1 if changed else 0)
