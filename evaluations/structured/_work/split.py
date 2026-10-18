"""Assign each family's split with the algorithm `cargo xtask gold validate-structured` checks
(xtask/src/gold/structured.rs): per repository, family ids sorted bytewise, shuffled by Fisher-Yates from the back
with SplitMix64 seeded 20261018 (j = next() % (i + 1)), the first four dev and the rest heldout.

  python -I evaluations/structured/_work/split.py evaluations/structured [--check]

Rewrites the `split` of every family in <dir>/*.json in place (key order and the rest of the file kept), or with
--check only reports differences and exits 1 if there are any.
"""
import json
import pathlib
import sys

SEED = 20261018
MASK = (1 << 64) - 1


def splitmix64(state):
    while True:
        state = (state + 0x9E3779B97F4A7C15) & MASK
        z = state
        z = ((z ^ (z >> 30)) * 0xBF58476D1CE4E5B9) & MASK
        z = ((z ^ (z >> 27)) * 0x94D049BB133111EB) & MASK
        yield z ^ (z >> 31)


def order(ids):
    ids = sorted(ids, key=lambda s: s.encode("utf-8"))
    rng = splitmix64(SEED)
    for i in range(len(ids) - 1, 0, -1):
        j = next(rng) % (i + 1)
        ids[i], ids[j] = ids[j], ids[i]
    return ids


def main():
    root = pathlib.Path(sys.argv[1])
    check = "--check" in sys.argv[2:]
    diffs = 0
    for path in sorted(root.glob("*.json")):
        gold = json.loads(path.read_text(encoding="utf-8"))
        dev = set(order([f["id"] for f in gold["families"]])[:4])
        for f in gold["families"]:
            want = "dev" if f["id"] in dev else "heldout"
            if f["split"] != want:
                diffs += 1
                print(f"{path.name}: {f['id']}: {f['split']} -> {want}")
                f["split"] = want
        if not check:
            path.write_text(json.dumps(gold, indent=2, ensure_ascii=False) + "\n", encoding="utf-8", newline="\n")
    print(f"{diffs} split(s) {'differ' if check else 'changed'}")
    sys.exit(1 if check and diffs else 0)


if __name__ == "__main__":
    main()
