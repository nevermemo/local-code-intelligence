## Bench comparison

Base: a730ef1e6a38 (stored baseline (levels B, C)) at 2026-10-12T18:08:05Z; new: 6f73fb925e0f (spike-int 6f73fb9 (levels B, C, defs probe)) at 2026-10-13T05:27:53Z. Bench set "v1" / "v1". Embedder: "lci-bench-fake-hash-512" / "lci-bench-fake-hash-512".

| Arm | Pairs | Errored new / base | Unpaired base / new | Same files | W/L/T (rr) | Metric | Base | New | Delta new-base [95% CI] |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| fusion | 60 | 0 / 0 | 0 / 0 | 60 | 0/0/60 | rr (primary) | 0.5456 | 0.5456 | +0.0000 [+0.0000, +0.0000] |
| | | | | | | hit10 | 0.8167 | 0.8167 | +0.0000 [+0.0000, +0.0000] |
| | | | | | | recall10 | 0.7667 | 0.7667 | +0.0000 [+0.0000, +0.0000] |
| grep | 60 | 0 / 0 | 0 / 0 | 60 | 0/0/60 | rr (primary) | 0.6368 | 0.6368 | +0.0000 [+0.0000, +0.0000] |
| | | | | | | hit10 | 0.8333 | 0.8333 | +0.0000 [+0.0000, +0.0000] |
| | | | | | | recall10 | 0.7694 | 0.7694 | +0.0000 [+0.0000, +0.0000] |

| Level B | Base | New | Change |
| --- | --- | --- | --- |
| index size (bytes, all repos) | 92916600 | 93553968 | +0.7% |
| update p50 (ms) | 1446.8 | 1370.5 | -5.3% |
| update p95 (ms) | 2243.3 | 2537.8 | +13.1% |
| update max (ms) | 2439.9 | 2747.7 | +12.6% |
| fusion total_ms p50 | 533.8 | 473.3 | -11.3% |
| fusion total_ms p95 | 1358.0 | 1165.2 | -14.2% |
| fusion total_ms max | 1512.9 | 1528.4 | +1.0% |
| grep total_ms p50 | 446.1 | 392.2 | -12.1% |
| grep total_ms p95 | 1221.0 | 1093.2 | -10.5% |
| grep total_ms max | 1617.0 | 1210.5 | -25.1% |
| lookup recall@5 / precision@5 (proxy, n=119 / 119) | 0.681 / 0.136 | 0.681 / 0.136 | reported only |
| defs hit@5 / precision@5 (sym.defs probe) | unavailable | 0.218 / 0.183 (n=119, errors=0) | reported only |
| defs black | unavailable | 0.000 / 0.000 (n=46, errors=0) | reported only |
| defs caddy | unavailable | 0.000 / 0.000 (n=36, errors=0) | reported only |
| defs tokio | unavailable | 0.703 / 0.588 (n=37, errors=0) | reported only |

| fusion rr by group (descriptive) | n | Base | New |
| --- | --- | --- | --- |
| repo black | 20 | 0.5050 | 0.5050 |
| repo caddy | 20 | 0.6092 | 0.6092 |
| repo tokio | 20 | 0.5225 | 0.5225 |
| tag keyword | 42 | 0.6024 | 0.6024 |
| tag keyword-agnostic | 18 | 0.4130 | 0.4130 |
| tag names-symbol | 14 | 0.7488 | 0.7488 |

### Verdict

No regression under the pre-registered rules.
- note: runs started more than 6 h apart: costs are printed, not judged (rule 4)

<!-- Added by hand on 2026-10-13, not compare output. -->
Cost (owner decision, 2026-10-13): this run and the baseline ran in different containers with the same machine fingerprint, so costs are reported and never judged under rule 4.
