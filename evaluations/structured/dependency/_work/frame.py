"""Sampling frame of the dependency stratum (item 2 slice 2a, second stratum).

Deterministic and re-runnable. Per repository, for every dependency declaration in the tracked, non-lockfile
manifests at the pinned revision (files are read from git blobs, never from a working tree):

  package.json     dependencies / devDependencies / peerDependencies / optionalDependencies
  Cargo.toml       [dependencies] [dev-dependencies] [build-dependencies] [workspace.dependencies], also under
                   [target.<cfg>.*] and as [<table>.<name>] sub-tables
  pyproject.toml   project.dependencies, project.optional-dependencies.*, dependency-groups.*, build-system.requires
  manifest.json    requirements

A package name is kept only if live grep could search it (plain identifier, at least 6 characters, not in
COMMON_NAMES of lci-maps/src/live_grep.rs at the lci commit) and the premise ripgrep command

  rg --no-config -n -w --hidden -g '!.git' --sort path --max-filesize 1M -- <name> .

run at the tree root prints at least one line from one of the 11 lockfile basenames among its first 50 output lines.
(To save work, rg is only run for names that occur as a whole word in some lockfile of the tree; that is a necessary
condition for the filter, so the frame is unchanged by it.) The kept names are ordered by SplitMix64 seed 20261018
over the names sorted bytewise (the same shuffle as split.py).

  python -I frame.py --lci-git <lci worktree> --lci-tree <clean export of the lci commit>
      --vite-git <bare repo> --vite-tree <export> --ha-git <bare repo> --ha-tree <export> --rg <rg> --out frame.json

All paths are arguments; the output holds repo-relative paths only.
"""
import argparse
import json
import pathlib
import re
import subprocess
import sys
import tomllib
from concurrent.futures import ThreadPoolExecutor

SEED = 20261018
MASK = (1 << 64) - 1
REVS = {
    "lci": "579a5bec69309d98c78825654aabe8f03ab64421",
    "vite": "7a89794331526a67bc9b29047b86957c581c128c",
    "home-assistant": "bd92409b6ade6e2873cac603104f23e5d9e7ec61",
}
LOCKFILES = {
    "package-lock.json", "npm-shrinkwrap.json", "composer.lock", "Cargo.lock", "yarn.lock", "pnpm-lock.yaml",
    "Pipfile.lock", "poetry.lock", "uv.lock", "bun.lock", "bun.lockb",
}
MANIFESTS = {"package.json", "Cargo.toml", "pyproject.toml", "manifest.json"}
NPM_SECTIONS = ("dependencies", "devDependencies", "peerDependencies", "optionalDependencies")
CARGO_SECTIONS = ("dependencies", "dev-dependencies", "build-dependencies")
PLAIN = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*$")
MIN_LEN = 6
MAX_FILESIZE = 1 << 20


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


# ---------------------------------------------------------------- git blobs

class Git:
    def __init__(self, base, rev):
        self.base, self.rev = list(base), rev

    def run(self, *args):
        return subprocess.check_output(self.base + list(args))

    def paths(self):
        out = self.run("ls-tree", "-r", "-z", "--name-only", self.rev)
        return [p.decode("utf-8") for p in out.split(b"\0") if p]

    def blob(self, path):
        return self.run("show", f"{self.rev}:{path}")


def text_of(raw):
    return raw.decode("utf-8-sig", errors="replace").replace("\r\n", "\n")


# ------------------------------------------------- JSON with line numbers

class JsonLines:
    """Minimal JSON parser that records, for object members and array elements, the 1-based line where the key
    (or element) starts and the line where its value ends. Raises on invalid JSON."""

    def __init__(self, text):
        self.s, self.i, self.line = text, 0, 1

    def ws(self):
        while self.i < len(self.s) and self.s[self.i] in " \t\r\n":
            if self.s[self.i] == "\n":
                self.line += 1
            self.i += 1

    def parse(self):
        self.ws()
        node = self.value()
        self.ws()
        if self.i != len(self.s):
            raise ValueError("trailing data")
        return node

    def value(self):
        c = self.s[self.i]
        start = self.line
        if c == "{":
            self.i += 1
            members = {}
            self.ws()
            if self.s[self.i] == "}":
                self.i += 1
                return ("obj", members, start, self.line)
            while True:
                self.ws()
                kline = self.line
                key = self.string()
                self.ws()
                if self.s[self.i] != ":":
                    raise ValueError("expected :")
                self.i += 1
                self.ws()
                members[key] = (kline, self.value())
                self.ws()
                c = self.s[self.i]
                self.i += 1
                if c == "}":
                    return ("obj", members, start, self.line)
                if c != ",":
                    raise ValueError("expected , or }")
        if c == "[":
            self.i += 1
            items = []
            self.ws()
            if self.s[self.i] == "]":
                self.i += 1
                return ("arr", items, start, self.line)
            while True:
                self.ws()
                items.append(self.value())
                self.ws()
                c = self.s[self.i]
                self.i += 1
                if c == "]":
                    return ("arr", items, start, self.line)
                if c != ",":
                    raise ValueError("expected , or ]")
        if c == '"':
            v = self.string()
            return ("str", v, start, self.line)
        j = self.i
        while j < len(self.s) and self.s[j] not in ",]} \t\r\n":
            j += 1
        tok = self.s[self.i:j]
        self.i = j
        json.loads(tok)
        return ("lit", tok, start, self.line)

    def string(self):
        if self.s[self.i] != '"':
            raise ValueError("expected string")
        j = self.i + 1
        while self.s[j] != '"':
            j += 2 if self.s[j] == "\\" else 1
        raw = self.s[self.i:j + 1]
        self.i = j + 1
        return json.loads(raw)


# ----------------------------------------------------------- requirements

REQ = re.compile(r"^\s*([A-Za-z0-9][A-Za-z0-9._-]*)")


def req_name(spec):
    m = REQ.match(spec)
    return m.group(1) if m else None


# ------------------------------------------------------------ declarations

def decls_package_json(path, text):
    root = JsonLines(text).parse()
    if root[0] != "obj":
        return []
    out = []
    for section in NPM_SECTIONS:
        if section not in root[1]:
            continue
        _, node = root[1][section]
        if node[0] != "obj":
            continue
        for name, (kline, val) in node[1].items():
            if val[0] == "str":
                out.append({"name": name, "path": path, "line": kline, "end_line": val[3], "section": section,
                            "value": val[1]})
    return out


def decls_manifest_json(path, text):
    root = JsonLines(text).parse()
    if root[0] != "obj" or "requirements" not in root[1]:
        return []
    _, node = root[1]["requirements"]
    out = []
    if node[0] == "arr":
        for item in node[1]:
            if item[0] == "str":
                name = req_name(item[1])
                if name:
                    out.append({"name": name, "path": path, "line": item[2], "end_line": item[3],
                                "section": "requirements", "value": item[1]})
    return out


TABLE = re.compile(r"^\s*\[\s*([^\[\]]+?)\s*\]\s*(#.*)?$")
KEYVAL = re.compile(r'^\s*("([^"]+)"|\'([^\']+)\'|([A-Za-z0-9_-]+))\s*=')


def split_table(header):
    """Dotted TOML table header to a list of parts (quotes honoured)."""
    parts, cur, quote = [], "", None
    for ch in header:
        if quote:
            if ch == quote:
                quote = None
            else:
                cur += ch
        elif ch in "\"'":
            quote = ch
        elif ch == ".":
            parts.append(cur.strip())
            cur = ""
        else:
            cur += ch
    parts.append(cur.strip())
    return parts


def cargo_section(parts):
    """(section label, sub-table dependency name or None) for a dependency table or sub-table, else (None, None)."""
    if parts[:2] == ["workspace", "dependencies"] and len(parts) <= 3:
        return "workspace.dependencies", parts[2] if len(parts) == 3 else None
    if parts[0] in CARGO_SECTIONS and len(parts) <= 2:
        return parts[0], parts[1] if len(parts) == 2 else None
    if parts[0] == "target" and len(parts) in (3, 4) and parts[2] in CARGO_SECTIONS:
        return "target." + parts[2], parts[3] if len(parts) == 4 else None
    return None, None


def decls_cargo(path, text):
    lines = text.split("\n")
    out, section, sub, sub_start = [], None, None, None

    def close_sub(upto):
        nonlocal sub, sub_start
        if sub:
            end = upto
            while end > sub_start and not lines[end - 1].strip():
                end -= 1
            out.append({"name": sub, "path": path, "line": sub_start, "end_line": end, "section": section,
                        "value": None, "table_form": True})
        sub = sub_start = None

    for n, line in enumerate(lines, 1):
        m = TABLE.match(line)
        if m and not line.lstrip().startswith("[["):
            close_sub(n - 1)
            section, sub = cargo_section(split_table(m.group(1)))
            if sub:
                sub_start = n
            continue
        if line.lstrip().startswith("[["):
            close_sub(n - 1)
            section = None
            continue
        if section and not sub:
            k = KEYVAL.match(line)
            if k:
                name = k.group(2) or k.group(3) or k.group(4)
                # extend over a multi-line inline value
                end, depth = n, 0
                rest = line.split("=", 1)[1]
                depth = rest.count("{") + rest.count("[") - rest.count("}") - rest.count("]")
                while depth > 0 and end < len(lines):
                    end += 1
                    depth += lines[end - 1].count("{") + lines[end - 1].count("[") - lines[end - 1].count("}") - lines[end - 1].count("]")
                out.append({"name": name, "path": path, "line": n, "end_line": end, "section": section,
                            "value": rest.strip()})
    close_sub(len(lines))
    # Cross-check against the TOML parser: every name found must be a real dependency key.
    data = tomllib.loads(text)
    keys = set()

    def walk(tbl, prefix):
        for k, v in tbl.items():
            if k in CARGO_SECTIONS and isinstance(v, dict):
                keys.update(v)
            elif k == "dependencies" and prefix == "workspace" and isinstance(v, dict):
                keys.update(v)
            elif k == "workspace" and isinstance(v, dict):
                walk(v, "workspace")
            elif k == "target" and isinstance(v, dict):
                for cfg in v.values():
                    if isinstance(cfg, dict):
                        walk(cfg, "target")
    walk(data, "")
    found = {d["name"] for d in out}
    assert found == keys, (path, sorted(found ^ keys))
    return out


def decls_pyproject(path, text):
    data = tomllib.loads(text)
    specs = []
    proj = data.get("project", {})
    specs += [("project.dependencies", s) for s in proj.get("dependencies", [])]
    for group, items in proj.get("optional-dependencies", {}).items():
        specs += [(f"project.optional-dependencies.{group}", s) for s in items]
    for group, items in data.get("dependency-groups", {}).items():
        specs += [(f"dependency-groups.{group}", s) for s in items if isinstance(s, str)]
    specs += [("build-system.requires", s) for s in data.get("build-system", {}).get("requires", [])]
    lines = text.split("\n")
    out, used = [], {}
    for section, spec in specs:
        name = req_name(spec)
        if not name:
            continue
        hits = [n for n, l in enumerate(lines, 1) if f'"{spec}"' in l or f"'{spec}'" in l]
        k = used.get(spec, 0)
        used[spec] = k + 1
        line = hits[k] if k < len(hits) else None
        out.append({"name": name, "path": path, "line": line, "end_line": line, "section": section, "value": spec})
    return out


PARSERS = {"package.json": decls_package_json, "manifest.json": decls_manifest_json, "Cargo.toml": decls_cargo,
           "pyproject.toml": decls_pyproject}


def common_names(git):
    src = git.blob("lci-maps/src/live_grep.rs").decode("utf-8")
    block = re.search(r"const COMMON_NAMES: &\[&str\] = &\[(.*?)\];", src, re.S).group(1)
    names = re.findall(r'"([^"]+)"', block)
    assert len(names) >= 40
    return set(names)


# ---------------------------------------------------------------------- rg

def run_rg(rg, root, name):
    cmd = [rg, "--no-config", "-n", "-w", "--hidden", "-g", "!.git", "--sort", "path", "--max-filesize", "1M",
           "--", name, "."]
    p = subprocess.run(cmd, cwd=root, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    assert p.returncode in (0, 1), (name, p.returncode, p.stderr[:200])
    return p.stdout.split(b"\n")[:-1] if p.stdout else []


def rg_hit(line):
    m = re.match(rb"^\.[/\\](.*?):(\d+):", line)
    if not m:
        return None
    return m.group(1).decode("utf-8", errors="replace").replace("\\", "/"), int(m.group(2))


def lockfile_stats(lines):
    paths = [rg_hit(l) for l in lines]
    is_lock = [bool(h) and h[0].rsplit("/", 1)[-1] in LOCKFILES for h in paths]
    return {"total_lines": len(lines), "lockfile_lines": sum(is_lock), "lockfile_lines_in_first_50": sum(is_lock[:50])}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--lci-git", required=True, help="a worktree or repository of lci containing the commit")
    ap.add_argument("--lci-tree", required=True, help="clean export of the lci commit")
    ap.add_argument("--vite-git", required=True)
    ap.add_argument("--vite-tree", required=True)
    ap.add_argument("--ha-git", required=True)
    ap.add_argument("--ha-tree", required=True)
    ap.add_argument("--rg", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--jobs", type=int, default=4)
    a = ap.parse_args()
    gits = {
        "lci": Git(["git", "-C", a.lci_git], REVS["lci"]),
        "vite": Git(["git", "--git-dir=" + a.vite_git], REVS["vite"]),
        "home-assistant": Git(["git", "--git-dir=" + a.ha_git], REVS["home-assistant"]),
    }
    trees = {"lci": a.lci_tree, "vite": a.vite_tree, "home-assistant": a.ha_tree}
    common = common_names(gits["lci"])
    result = {"seed": SEED, "revisions": REVS, "min_length": MIN_LEN, "repos": {}}
    for repo in ("lci", "vite", "home-assistant"):
        git, root = gits[repo], trees[repo]
        paths = git.paths()
        lockfiles = [p for p in paths if p.rsplit("/", 1)[-1] in LOCKFILES]
        manifests = [p for p in paths if p.rsplit("/", 1)[-1] in MANIFESTS]
        decls, skipped_manifests = [], []
        for p in manifests:
            base = p.rsplit("/", 1)[-1]
            try:
                decls += PARSERS[base](p, text_of(git.blob(p)))
            except (ValueError, tomllib.TOMLDecodeError, IndexError) as e:
                skipped_manifests.append({"path": p, "error": type(e).__name__})
        by_name = {}
        for d in decls:
            by_name.setdefault(d["name"], []).append(d)
        plain = {n for n in by_name if PLAIN.match(n)}
        long_enough = {n for n in plain if len(n) >= MIN_LEN}
        searchable = {n for n in long_enough if n not in common}
        # necessary condition for a lockfile line in rg's output: a whole-word match in some lockfile
        lock_text = []
        for p in lockfiles:
            f = pathlib.Path(root) / p
            if f.is_file() and f.stat().st_size <= MAX_FILESIZE:
                lock_text.append(f.read_bytes().decode("utf-8", errors="replace"))
        candidates = sorted((n for n in searchable
                             if any(re.search(r"(?<!\w)" + re.escape(n) + r"(?!\w)", t) for t in lock_text)),
                            key=lambda s: s.encode("utf-8"))
        with ThreadPoolExecutor(a.jobs) as pool:
            outputs = list(pool.map(lambda n: run_rg(a.rg, root, n), candidates))
        stats = {n: lockfile_stats(o) for n, o in zip(candidates, outputs)}
        frame = sorted((n for n in candidates if stats[n]["lockfile_lines_in_first_50"] >= 1),
                       key=lambda s: s.encode("utf-8"))
        ordered = order(frame)
        result["repos"][repo] = {
            "tracked_paths": len(paths),
            "lockfiles": lockfiles,
            "manifests": len(manifests),
            "manifests_unparsed": skipped_manifests,
            "declarations": len(decls),
            "distinct_names": len(by_name),
            "plain_names": len(plain),
            "plain_len_ge_6": len(long_enough),
            "searchable_not_common": len(searchable),
            "lockfile_word_prefilter": len(candidates),
            "frame_size": len(frame),
            "order": [{
                "name": n,
                "rg": stats[n],
                "declarations": [{k: v for k, v in d.items() if k != "name"} for d in by_name[n]],
            } for n in ordered],
        }
        print(repo, {k: v for k, v in result["repos"][repo].items() if k not in ("order", "lockfiles")},
              file=sys.stderr)
    pathlib.Path(a.out).write_text(json.dumps(result, indent=1, ensure_ascii=False) + "\n", encoding="utf-8",
                                   newline="\n")


if __name__ == "__main__":
    main()
