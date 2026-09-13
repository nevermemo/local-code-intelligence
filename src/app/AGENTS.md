# App coordination guidance

Keep `App` as the public facade. Preserve index lifecycle metadata, stale-index
reuse, per-workspace single-flight coordination, and independent fail-open
retrieval channels. A structural split must preserve public paths and serialized
responses. Do not let readiness or tests contact port 8765.
