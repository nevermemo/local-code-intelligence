## Bench comparison

Base: b87b16a40917 (S2 b87b16a (levels B, C)) at 2026-10-13T04:46:24Z; new: b466f7f1d0aa (S3a b466f7f (levels B, C, defs probe)) at 2026-10-13T04:57:45Z. Bench set "v1" / "v1". Embedder: "lci-bench-fake-hash-512" / "lci-bench-fake-hash-512".

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
| index size (bytes, all repos) | 93850342 | 92826031 | -1.1% |
| update p50 (ms) | 1329.3 | 1434.4 | +7.9% |
| update p95 (ms) | 2497.4 | 2354.2 | -5.7% |
| update max (ms) | 2643.8 | 2510.4 | -5.0% |
| fusion total_ms p50 | 453.8 | 492.7 | +8.6% |
| fusion total_ms p95 | 1160.9 | 1176.9 | +1.4% |
| fusion total_ms max | 1385.5 | 1401.3 | +1.1% |
| grep total_ms p50 | 385.1 | 415.2 | +7.8% |
| grep total_ms p95 | 1079.5 | 1034.2 | -4.2% |
| grep total_ms max | 1228.4 | 1144.0 | -6.9% |
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

