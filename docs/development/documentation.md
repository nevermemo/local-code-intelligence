# Documentation rules

These rules apply to every Markdown file in this repository and to the comments in `config.example.toml`.
Agents read these files without chat context and pay for every token, so the files are written for them first.

1. Facts first. State what the code does today, in the present tense, before any history or rationale.
2. One place per fact. A fact has one home; every other file points to that home instead of restating it.
   Homes: behaviour and surfaces in `README.md`; module structure in `docs/architecture.md`; build and test in
   `docs/development/testing.md`; machine values in `docs/development/machine-values.md`; traversal engine in
   `docs/spike/multimap/engine.md`; measurement method in `docs/spike/multimap/evaluation.md`; plans in
   `docs/development/roadmap.md`; decisions, findings and history in `docs/decisions.md`.
3. Sections stand alone. An `##` section can be loaded by itself: it names what it is about in its first line and
   does not depend on text after it. No forward references inside a section.
4. Plans are marked. A file that holds plans starts with `Status: PLAN`. A section that describes a design that is
   not built says so in its heading or first line. Frozen records (pre-registrations, results, generated files)
   start with their date and are never edited; corrections are appended as dated notes or recorded in
   `docs/decisions.md`.
5. No narrative history outside `docs/decisions.md`. A fact that changed is replaced, and the change is one line in
   the log with its date and evidence.
6. Numbers carry a date and a source: the commit, the results file or the run name. A number without both is an
   estimate and says so.
7. Code references name symbols and files, never line numbers.
8. No machine-specific values: no user profile paths, drive letters, client session folders, local port layouts
   other than the product's own defaults, model aliases of one machine, or secrets. `cargo xtask lint-machine-paths`
   enforces part of this; the rest is reviewed.
9. Links are relative and resolve; anchors match a heading in the target. No link checker is checked in yet (open work:
   roadmap N92, an `xtask` subcommand). Until it lands, check every relative link and anchor in the files you
   touched by hand, or with a script of your own kept outside the repository; none may be missing.
10. Short over complete: a table over prose for facts with the same shape; one example instead of three; a pointer
    instead of a copy. Target sizes: README <= 260 lines, any guide <= 250, the roadmap <= 350, the log <= 300.
11. Vendored third-party files (`.agents/skills/rust-skills/**`) are not edited.
12. Every change to these files is checked against the code it describes (names, defaults, ports, commands) in the
    same change.
