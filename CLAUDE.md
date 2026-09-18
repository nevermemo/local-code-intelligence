# Claude Code entry point

Before doing any non-trivial work in this repository, read `AGENTS.md` at the
repository root. It defines the product boundaries, working method, and the
project skills under `.agents/skills/` (`lci-language-adapter`,
`lci-lsp-adapter`, `lci-retrieval-evaluation`, `lci-release-validation`).

Load the skill matching the task before starting it -- in particular,
`.agents/skills/lci-language-adapter/references/acceptance.md` defines the
full acceptance bar a language must pass before it can be called supported
(parser/chunk, incremental reuse, deletion reconciliation, restart reuse,
retrieval, decoy-ranking, and documentation updates). Do not add or claim
language support without checking that file first.

For the fast `lci-core`-scoped build/test loop, see
`docs/development/testing.md`.
