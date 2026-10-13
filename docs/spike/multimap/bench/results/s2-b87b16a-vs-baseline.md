## Bench comparison

Base: a730ef1e6a38 (stored baseline (levels B, C)) at 2026-10-12T18:08:05Z; new: b87b16a40917 (S2 b87b16a (levels B, C)) at 2026-10-13T04:46:24Z. Bench set "v1" / "v1". Embedder: "lci-bench-fake-hash-512" / "lci-bench-fake-hash-512".

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
| index size (bytes, all repos) | 92916600 | 93850342 | +1.0% |
| update p50 (ms) | 1446.8 | 1329.3 | -8.1% |
| update p95 (ms) | 2243.3 | 2497.4 | +11.3% |
| update max (ms) | 2439.9 | 2643.8 | +8.4% |
| fusion total_ms p50 | 533.8 | 453.8 | -15.0% |
| fusion total_ms p95 | 1358.0 | 1160.9 | -14.5% |
| fusion total_ms max | 1512.9 | 1385.5 | -8.4% |
| grep total_ms p50 | 446.1 | 385.1 | -13.7% |
| grep total_ms p95 | 1221.0 | 1079.5 | -11.6% |
| grep total_ms max | 1617.0 | 1228.4 | -24.0% |
| lookup recall@5 / precision@5 (proxy, n=119 / 119) | 0.681 / 0.136 | 0.681 / 0.136 | reported only |

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
