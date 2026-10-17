# M0b results: fused and reranked LCI on `impact`, `where-to` and `flow`

Results of the run pre-registered in `m0b-prereg.md` (sections 1 to 10, amended in sections 11 and 12, all
committed before the run that counts). One full run, 2026-10-14. Everything in the tables is [M] measured unless a
line says [I] inferred or [G] guess. Paths are written `<gold>`, `<tree>` and `<scratch>`. This is a **retrieval
ablation** on a frozen tree (ranked chunks in, gold scorers out). It is not an agent result: nothing here says what an
agent does with these results. "F", "FR" and "S" always name the ablation arms of the pre-registration; "L" is M0.3's
lexical-only arm. Nothing in the slice is a gate.

## 1. What was run

| item | value |
| --- | --- |
| Tree | `<tree>` at `bfaff446316636e2089e0440bfcf127394950c42`, as M0.3. |
| Gold | `<gold>/impact.json` sha256 `8c1182136ea530ee737f1780e800b3afc3e33bbcb97020fddbd7956cebb52314` (141 primary items); `<gold>/flow.json` `2c3a514c1e325225f3fb88038db1c4eae495cd9e29d33215c1f424c9b25ed62a` (111 items, 55 headline); `<gold>/where-to.json` `5f0f0e7fd8094ece73a5f9fa25e5c92c2b305f7f9fc12a20d777dea4546903a8` (59 items). All three were checked at run start and equal the pre-registered ones. |
| Binary | Release build of branch `spike/v2-m0b` made at commit `b2fa472` (`git diff 2b8db07 HEAD` is empty for `src/`, `lci-core/`, `xtask/`, `Cargo.toml` and `Cargo.lock`, so it is M0.3's code); no rebuild between the exploratory run and this one. sha256 `313e0f9dae7854fabdcb46e1727174f3644bfb9f291ba2789612673dbe7c8515`. `rg` 15.2.0 from `PATH`. |
| Scorers | `cargo xtask gold` of the same branch (debug build); definitions made by `cargo xtask gold to-evaluate` are byte-identical to M0.3's (141, 111, 59 queries; `top_k` 20, 10, 10). |
| Embedder (amendment 12) | `qwen3-embedding-4b-q8` on `http://127.0.0.1:8766/v1`: Qwen3-Embedding-4B, Q8_0 GGUF, llama.cpp `b11609-fcac19079` CUDA build on an RTX 5090, dimension 2560 [M, from the service]. Start command, machine values replaced: `llama-server -m <models>/Qwen3-Embedding-4B-Q8_0.gguf --embedding --pooling last --alias qwen3-embedding-4b-q8 --host 127.0.0.1 --port 8766 -ngl 99 -fa on -c 32768 --parallel 8 --no-kv-unified -b 4096 -ub 4096 --cache-ram 0`. Prompt templates at their defaults (instruction prefix for the query, bare code for chunks), batch size 8. |
| Reranker | `qwen3-reranker-4b` on `http://127.0.0.1:8767/rerank`: Qwen3-Reranker-4B, Q8_0 GGUF, llama.cpp `b11609-fcac19079` CUDA build on the same RTX 5090 [M, from the service]. Start command: `llama-server -m <models>/qwen3-reranker-4b-q8_0.gguf --rerank --alias qwen3-reranker-4b --host 127.0.0.1 --port 8767 -ngl 99 -fa on -c 32768 --parallel 8 --no-kv-unified -b 4096 -ub 4096 --cache-ram 0`. The earlier text of the pre-registration's section 11 (a second GPU on port 8773) was superseded by the port move; no query of this slice was sent to 8773. The same GGUF file previously served from the iGPU (llama.cpp ROCm build `b1342`); an equivalence check between the two servers (12 queries) gave mean Spearman 0.985 and the same top-1 in 11 of 12 [G, reported by the coordinator, not re-run here]. |
| Configuration | One file per arm, sharing `data_dir = <scratch>/data-m0b-4b`; `[channels]` L check: semantic off, lexical on; S: semantic on, lexical off; F: both on; FR: both on and reranker on; LSP off in all; `index.freshness = manual`; `rerank_candidate_count = 24`, `rerank_document_max_bytes = 4096`, `reranker_timeout_seconds = 240`, `embedding_timeout_seconds = 120`, `rrf_k = 60`, fusion weights 1.0, `max_chunks_per_file = 2`, `lexical_candidate_count = semantic_candidate_count = 40`; no overlay (`--no-local-config`), no `LCI_*` variables. These equal the A/B's frozen values (read from its configuration file) except the embedder, see amendment 12. |
| Index | Built once into `<scratch>/data-m0b-4b` by `lci index <tree>`: 86 files, 2,085 chunks, 1,756 embedded (identical chunk text is embedded once), 76.3 s of indexing time, 2026-10-14 13:10:54Z to 13:12:14Z. |
| Queries | 141 + 111 + 59, the item `question` verbatim, one at a time. Converter C1 unchanged from M0.3 (`impact`: 2,820 answers for 141 items, 20 per item, in S, F and FR; `flow`: 562 / 589 / 597 function names for S / F / FR, with 1 / 1 / 2 items that have no `fn` in their chunks (L: 566, 1); `where-to`: 590 answers, 10 per item). |
| Reference arms | `grep_baseline` (stored per item, `cargo xtask gold baseline`) and the file-level BM25, both reused from M0.3 (not re-run). |

Per-item scoring: every metric the scorer defines on a cell was reproduced by the mean of the per-item values (0
mismatches in all nine scored runs of the amended run; `null` metrics skipped).

### L identity check [M]

The M0.3 lexical-only configuration was run over all 311 queries on the new index (no model service called at
query time): **0 of 311 ranked chunk lists differ** from M0.3's (rank, path, start line, end line). L is valid as
reused: its numbers below are M0.3's. The chunk boundaries of the new index equal M0.3's for every returned chunk
[M]. Median query time 98 to 131 ms.

### FR validity [M]

Every one of the 311 FR queries returned `reranked = true` and no warning, none errored, none was skipped, and the
longest took 1.8 s wall clock. **Degraded queries: 0 of 311 before the retry pass and 0 after.** The retry pass was
therefore empty, "FR-as-returned" equals FR, and the validity limit (16 of 311) is not approached. FR is valid.

### Timings and load [M, not a latency claim]

The services were shared with a live A/B batch (and, as reported by the coordinator and not observed here, an index pre-build). At the start and end of every
`lci evaluate` call the `ab_sdk_run` python processes were counted: **2 each time** (one batch: the venv launcher and its
interpreter). Timings below are medians of the per-query evaluate-report timings, milliseconds.

| arm | set | wall clock of the call | median total | max total | median query embedding | median rerank | max rerank |
| --- | --- | --- | --- | --- | --- | --- | --- |
| L check | impact / flow / where-to | 20 s / 18 s / 9 s | 105 / 131 / 98 | 451 | 0 | 0 | 0 |
| S | impact / flow / where-to | 37 s / 23 s / 6 s | 220 / 205 / 66 | 542 | 168 / 157 / 28 | 0 | 0 |
| F | impact / flow / where-to | 35 s / 29 s / 16 s | 227 / 219 / 230 | 505 | 176 / 170 / 179 | 0 | 0 |
| FR | impact / flow / where-to | 180 s / 141 s / 68 s | 1,239 / 1,219 / 1,137 | 1,788 | 136 / 126 / 48 | 1,025 / 1,022 / 956 | 1,649 |

Reranking adds about 1.0 s per search at the median on the 5090 (window 24) under this load [M]. For comparison, the
exploratory iGPU-era run (section 7) had query embedding at a median of 0.6 to 1.3 s per query and a per-query total of up to 23.7 s.

## 2. `impact` (141 primary items)

Primary metric `decl_f1`, read by name-ambiguity band (rule 1 of M0.3). Intervals: percentile bootstrap over items,
10,000 resamples, seed 20261014 (Wilson where every per-item value is 0 or 1, as in M0.3). L is M0.3's run. The
`grep_baseline` columns are the stored per-item values averaged.

| arm | all (141) | unique (81) | some (25) | common (35) |
| --- | --- | --- | --- | --- |
| L | 0.153 [0.131, 0.175] | 0.183 [0.156, 0.213] | 0.115 [0.073, 0.160] | 0.109 [0.068, 0.156] |
| S | 0.118 [0.102, 0.134] | 0.124 [0.105, 0.144] | 0.098 [0.063, 0.135] | 0.118 [0.082, 0.158] |
| F | 0.145 [0.126, 0.164] | 0.164 [0.139, 0.189] | 0.124 [0.082, 0.170] | 0.117 [0.083, 0.153] |
| FR | 0.164 [0.144, 0.186] | 0.178 [0.153, 0.205] | 0.135 [0.089, 0.185] | 0.153 [0.108, 0.204] |
| `grep_baseline` | 0.817 | 0.995 | 0.792 | 0.421 |

Ranking and file level (each cell n as in the header; `coverage` and `kind` strata are in the scratch files and are
not read):

| metric | arm | all (141) | unique (81) | some (25) | common (35) |
| --- | --- | --- | --- | --- | --- |
| `decl_recall` | L | 0.354 [0.304, 0.405] | 0.439 [0.376, 0.506] | 0.283 [0.172, 0.410] | 0.206 [0.132, 0.285] |
| `decl_recall` | S | 0.268 [0.229, 0.308] | 0.307 [0.253, 0.364] | 0.196 [0.125, 0.272] | 0.228 [0.161, 0.299] |
| `decl_recall` | F | 0.338 [0.292, 0.384] | 0.405 [0.340, 0.473] | 0.274 [0.183, 0.369] | 0.228 [0.163, 0.295] |
| `decl_recall` | FR | 0.371 [0.325, 0.417] | 0.432 [0.371, 0.497] | 0.294 [0.200, 0.391] | 0.285 [0.208, 0.368] |
| `decl_recall` | `grep_baseline` | 0.988 | 0.992 | 0.966 | 0.992 |
| `decl_precision` | L | 0.108 [0.091, 0.126] | 0.130 [0.107, 0.156] | 0.079 [0.049, 0.111] | 0.080 [0.049, 0.115] |
| `decl_precision` | S | 0.085 [0.073, 0.098] | 0.088 [0.073, 0.103] | 0.073 [0.045, 0.105] | 0.087 [0.060, 0.117] |
| `decl_precision` | F | 0.103 [0.089, 0.118] | 0.116 [0.096, 0.137] | 0.088 [0.056, 0.125] | 0.086 [0.061, 0.113] |
| `decl_precision` | FR | 0.119 [0.102, 0.137] | 0.128 [0.106, 0.152] | 0.096 [0.061, 0.135] | 0.114 [0.080, 0.154] |
| `decl_recall@5` | L | 0.144 [0.110, 0.180] | 0.170 [0.128, 0.217] | 0.172 [0.075, 0.297] | 0.065 [0.027, 0.111] |
| `decl_recall@5` | S | 0.098 [0.073, 0.123] | 0.113 [0.080, 0.148] | 0.078 [0.032, 0.135] | 0.077 [0.034, 0.126] |
| `decl_recall@5` | F | 0.202 [0.168, 0.237] | 0.256 [0.209, 0.304] | 0.165 [0.095, 0.242] | 0.105 [0.057, 0.157] |
| `decl_recall@5` | FR | 0.232 [0.192, 0.273] | 0.288 [0.231, 0.349] | 0.143 [0.077, 0.221] | 0.165 [0.108, 0.226] |
| `decl_recall@10` | L | 0.232 [0.188, 0.276] | 0.281 [0.223, 0.342] | 0.218 [0.113, 0.344] | 0.128 [0.072, 0.192] |
| `decl_recall@10` | S | 0.163 [0.131, 0.196] | 0.191 [0.147, 0.238] | 0.115 [0.062, 0.177] | 0.134 [0.080, 0.192] |
| `decl_recall@10` | F | 0.282 [0.241, 0.323] | 0.340 [0.283, 0.399] | 0.219 [0.138, 0.305] | 0.191 [0.131, 0.254] |
| `decl_recall@10` | FR | 0.333 [0.290, 0.377] | 0.397 [0.337, 0.461] | 0.237 [0.162, 0.315] | 0.252 [0.188, 0.319] |
| `decl_ndcg@10` | L | 0.202 [0.166, 0.239] | 0.251 [0.202, 0.303] | 0.164 [0.092, 0.245] | 0.114 [0.062, 0.173] |
| `decl_ndcg@10` | S | 0.139 [0.115, 0.164] | 0.165 [0.133, 0.200] | 0.094 [0.054, 0.138] | 0.109 [0.066, 0.156] |
| `decl_ndcg@10` | F | 0.255 [0.220, 0.289] | 0.319 [0.273, 0.365] | 0.201 [0.128, 0.276] | 0.145 [0.099, 0.197] |
| `decl_ndcg@10` | FR | 0.292 [0.256, 0.326] | 0.342 [0.296, 0.390] | 0.204 [0.133, 0.281] | 0.237 [0.176, 0.302] |
| `file_f1` | L | 0.332 [0.303, 0.362] | 0.356 [0.315, 0.396] | 0.282 [0.233, 0.331] | 0.314 [0.251, 0.379] |
| `file_f1` | S | 0.351 [0.325, 0.378] | 0.332 [0.295, 0.370] | 0.365 [0.320, 0.413] | 0.384 [0.333, 0.433] |
| `file_f1` | F | 0.340 [0.317, 0.365] | 0.348 [0.314, 0.383] | 0.334 [0.295, 0.376] | 0.328 [0.283, 0.374] |
| `file_f1` | FR | 0.361 [0.336, 0.387] | 0.358 [0.324, 0.393] | 0.355 [0.314, 0.398] | 0.373 [0.317, 0.428] |
| `file_f1` | BM25 | 0.254 [0.226, 0.282] | 0.265 [0.228, 0.304] | 0.239 [0.177, 0.302] | 0.239 [0.181, 0.298] |
| `file_f1` | `grep_baseline` | 0.849 | 0.996 | 0.865 | 0.499 |
| `file_recall` | L | 0.790 [0.735, 0.841] | 0.857 [0.794, 0.916] | 0.665 [0.541, 0.786] | 0.723 [0.597, 0.838] |
| `file_recall` | S | 0.829 [0.788, 0.867] | 0.822 [0.763, 0.876] | 0.832 [0.745, 0.911] | 0.843 [0.761, 0.914] |
| `file_recall` | F | 0.876 [0.839, 0.909] | 0.907 [0.863, 0.947] | 0.847 [0.762, 0.925] | 0.822 [0.733, 0.901] |
| `file_recall` | FR | 0.900 [0.866, 0.931] | 0.923 [0.884, 0.958] | 0.864 [0.786, 0.933] | 0.872 [0.780, 0.950] |
| `file_recall` | BM25 | 0.541 [0.484, 0.595] | 0.570 [0.497, 0.642] | 0.532 [0.386, 0.681] | 0.480 [0.368, 0.593] |
| `file_recall` | `grep_baseline` | 0.991 | 0.994 | 0.970 | 1.000 |
| `file_precision` | L | 0.226 [0.202, 0.250] | 0.239 [0.207, 0.274] | 0.194 [0.156, 0.234] | 0.217 [0.169, 0.268] |
| `file_precision` | S | 0.237 [0.215, 0.260] | 0.223 [0.192, 0.256] | 0.248 [0.208, 0.292] | 0.262 [0.221, 0.302] |
| `file_precision` | F | 0.223 [0.204, 0.244] | 0.229 [0.200, 0.259] | 0.217 [0.185, 0.254] | 0.215 [0.180, 0.252] |
| `file_precision` | FR | 0.239 [0.218, 0.260] | 0.236 [0.207, 0.266] | 0.234 [0.199, 0.271] | 0.249 [0.205, 0.296] |
| `file_precision` | BM25 | 0.176 [0.155, 0.199] | 0.183 [0.155, 0.213] | 0.160 [0.119, 0.202] | 0.173 [0.124, 0.227] |
| `file_precision` | `grep_baseline` | 0.816 | 1.000 | 0.814 | 0.390 |

Paired differences, rule-2 verdicts computed mechanically (floor 0.05; 95% bootstrap interval of per-item
differences; counts are items improved / tied / worsened):

| `decl_f1` | overall | band=unique | band=some | band=common |
| --- | --- | --- | --- | --- |
| F minus L | -0.008 [-0.021, +0.005] no evidence (n 141; +44/=55/-42) | -0.020 [-0.038, -0.003] small loss (n 81; +24/=30/-27) | +0.009 [-0.015, +0.034] no evidence (n 25; +7/=13/-5) | +0.008 [-0.020, +0.036] no evidence (n 35; +13/=12/-10) |
| FR minus L | +0.012 [-0.002, +0.025] no evidence (n 141; +57/=56/-28) | -0.005 [-0.022, +0.010] no evidence (n 81; +28/=34/-19) | +0.021 [-0.000, +0.042] no evidence (n 25; +9/=14/-2) | +0.044 [+0.012, +0.077] small gain (n 35; +20/=8/-7) |
| FR minus F | +0.019 [+0.012, +0.028] small gain (n 141; +30/=111/-0) | +0.015 [+0.007, +0.024] small gain (n 81; +11/=70/-0) | +0.011 [+0.000, +0.025] small gain (n 25; +4/=21/-0) | +0.036 [+0.018, +0.058] small gain (n 35; +15/=20/-0) |
| S minus L | -0.035 [-0.056, -0.014] small loss (n 141; +47/=36/-58) | -0.059 [-0.086, -0.034] worse (n 81; +21/=20/-40) | -0.016 [-0.053, +0.019] no evidence (n 25; +7/=11/-7) | +0.009 [-0.037, +0.057] no evidence (n 35; +19/=5/-11) |
| S minus F | -0.027 [-0.042, -0.013] small loss (n 141; +28/=63/-50) | -0.039 [-0.061, -0.019] small loss (n 81; +14/=33/-34) | -0.026 [-0.050, -0.004] small loss (n 25; +4/=12/-9) | +0.001 [-0.025, +0.029] no evidence (n 35; +10/=18/-7) |
| F minus grep | -0.672 [-0.718, -0.623] worse (n 141; +4/=0/-137) | -0.832 [-0.856, -0.806] worse (n 81; +0/=0/-81) | -0.668 [-0.738, -0.595] worse (n 25; +0/=0/-25) | -0.304 [-0.397, -0.213] worse (n 35; +4/=0/-31) |
| FR minus grep | -0.652 [-0.700, -0.602] worse (n 141; +6/=0/-135) | -0.817 [-0.843, -0.790] worse (n 81; +0/=0/-81) | -0.657 [-0.730, -0.581] worse (n 25; +0/=0/-25) | -0.268 [-0.362, -0.175] worse (n 35; +6/=0/-29) |

Ranking metrics and window-level recall, F and FR against L and FR against F:

| `decl_recall@5` | overall | band=unique | band=some | band=common |
| --- | --- | --- | --- | --- |
| F minus L | +0.058 [+0.027, +0.087] beats (n 141; +51/=74/-16) | +0.086 [+0.044, +0.128] beats (n 81; +35/=37/-9) | -0.008 [-0.078, +0.052] no evidence (n 25; +7/=15/-3) | +0.039 [-0.007, +0.088] no evidence (n 35; +9/=22/-4) |
| FR minus L | +0.088 [+0.050, +0.124] beats (n 141; +62/=56/-23) | +0.118 [+0.072, +0.165] beats (n 81; +38/=30/-13) | -0.029 [-0.116, +0.049] no evidence (n 25; +5/=16/-4) | +0.100 [+0.028, +0.172] beats (n 35; +19/=10/-6) |
| FR minus F | +0.030 [+0.002, +0.059] small gain (n 141; +39/=77/-25) | +0.033 [-0.004, +0.071] no evidence (n 81; +22/=43/-16) | -0.021 [-0.087, +0.033] no evidence (n 25; +3/=18/-4) | +0.060 [+0.006, +0.118] beats (n 35; +14/=16/-5) |

| `decl_recall@10` | overall | band=unique | band=some | band=common |
| --- | --- | --- | --- | --- |
| F minus L | +0.050 [+0.011, +0.089] small gain (n 141; +54/=58/-29) | +0.059 [+0.003, +0.115] beats (n 81; +33/=30/-18) | +0.001 [-0.082, +0.081] no evidence (n 25; +6/=15/-4) | +0.063 [-0.008, +0.133] no evidence (n 35; +15/=13/-7) |
| FR minus L | +0.101 [+0.064, +0.139] beats (n 141; +74/=48/-19) | +0.117 [+0.070, +0.166] beats (n 81; +44/=28/-9) | +0.019 [-0.075, +0.108] no evidence (n 25; +10/=10/-5) | +0.124 [+0.046, +0.200] beats (n 35; +20/=10/-5) |
| FR minus F | +0.051 [+0.030, +0.074] beats (n 141; +39/=96/-6) | +0.058 [+0.028, +0.090] beats (n 81; +20/=57/-4) | +0.018 [-0.034, +0.068] no evidence (n 25; +5/=18/-2) | +0.061 [+0.033, +0.092] beats (n 35; +14/=21/-0) |

| `decl_ndcg@10` | overall | band=unique | band=some | band=common |
| --- | --- | --- | --- | --- |
| F minus L | +0.053 [+0.023, +0.084] beats (n 141; +77/=31/-33) | +0.068 [+0.023, +0.112] beats (n 81; +48/=14/-19) | +0.037 [-0.027, +0.103] no evidence (n 25; +9/=10/-6) | +0.032 [-0.014, +0.074] no evidence (n 35; +20/=7/-8) |
| FR minus L | +0.090 [+0.052, +0.128] beats (n 141; +86/=20/-35) | +0.091 [+0.042, +0.138] beats (n 81; +53/=8/-20) | +0.041 [-0.049, +0.137] no evidence (n 25; +11/=7/-7) | +0.124 [+0.046, +0.203] beats (n 35; +22/=5/-8) |
| FR minus F | +0.037 [+0.010, +0.064] small gain (n 141; +65/=30/-46) | +0.023 [-0.014, +0.059] no evidence (n 81; +33/=15/-33) | +0.004 [-0.053, +0.057] no evidence (n 25; +11/=7/-7) | +0.092 [+0.042, +0.145] beats (n 35; +21/=8/-6) |

| `decl_recall` | overall | band=unique | band=some | band=common |
| --- | --- | --- | --- | --- |
| F minus L | -0.016 [-0.048, +0.017] no evidence (n 141; +33/=70/-38) | -0.034 [-0.078, +0.012] no evidence (n 81; +15/=40/-26) | -0.009 [-0.083, +0.054] no evidence (n 25; +7/=15/-3) | +0.021 [-0.043, +0.087] no evidence (n 35; +11/=15/-9) |
| FR minus L | +0.018 [-0.016, +0.050] no evidence (n 141; +42/=74/-25) | -0.007 [-0.049, +0.036] no evidence (n 81; +18/=45/-18) | +0.011 [-0.062, +0.073] no evidence (n 25; +8/=15/-2) | +0.079 [+0.009, +0.148] beats (n 35; +16/=14/-5) |
| FR minus F | +0.033 [+0.021, +0.048] small gain (n 141; +24/=117/-0) | +0.027 [+0.011, +0.045] small gain (n 81; +10/=71/-0) | +0.020 [+0.000, +0.050] no evidence (n 25; +2/=23/-0) | +0.057 [+0.028, +0.090] beats (n 35; +12/=23/-0) |

File level, including the file-level BM25:

| `file_f1` | overall | band=unique | band=some | band=common |
| --- | --- | --- | --- | --- |
| F minus L | +0.008 [-0.008, +0.024] no evidence (n 141; +58/=26/-57) | -0.008 [-0.026, +0.010] no evidence (n 81; +27/=17/-37) | +0.052 [+0.007, +0.097] beats (n 25; +18/=1/-6) | +0.014 [-0.018, +0.050] no evidence (n 35; +13/=8/-14) |
| FR minus L | +0.029 [+0.013, +0.045] small gain (n 141; +70/=21/-50) | +0.002 [-0.016, +0.020] no evidence (n 81; +29/=11/-41) | +0.072 [+0.035, +0.112] beats (n 25; +19/=1/-5) | +0.059 [+0.027, +0.095] beats (n 35; +22/=9/-4) |
| FR minus F | +0.020 [+0.013, +0.028] small gain (n 141; +73/=32/-36) | +0.010 [+0.002, +0.019] small gain (n 81; +32/=21/-28) | +0.021 [+0.006, +0.036] small gain (n 25; +15/=5/-5) | +0.045 [+0.027, +0.062] small gain (n 35; +26/=6/-3) |
| F minus bm25 | +0.086 [+0.058, +0.115] beats (n 141; +88/=3/-50) | +0.083 [+0.044, +0.121] beats (n 81; +50/=2/-29) | +0.095 [+0.034, +0.159] beats (n 25; +15/=0/-10) | +0.089 [+0.040, +0.140] beats (n 35; +23/=1/-11) |
| FR minus bm25 | +0.107 [+0.078, +0.136] beats (n 141; +99/=3/-39) | +0.092 [+0.054, +0.131] beats (n 81; +54/=1/-26) | +0.116 [+0.051, +0.181] beats (n 25; +18/=1/-6) | +0.134 [+0.079, +0.191] beats (n 35; +27/=1/-7) |

| `file_recall` | overall | band=unique | band=some | band=common |
| --- | --- | --- | --- | --- |
| F minus L | +0.086 [+0.049, +0.125] beats (n 141; +35/=95/-11) | +0.050 [+0.004, +0.102] beats (n 81; +12/=62/-7) | +0.182 [+0.094, +0.283] beats (n 25; +13/=11/-1) | +0.099 [+0.030, +0.172] beats (n 35; +10/=22/-3) |
| FR minus L | +0.110 [+0.073, +0.149] beats (n 141; +38/=97/-6) | +0.066 [+0.021, +0.117] beats (n 81; +13/=63/-5) | +0.199 [+0.111, +0.300] beats (n 25; +13/=12/-0) | +0.149 [+0.073, +0.230] beats (n 35; +12/=22/-1) |
| FR minus F | +0.024 [+0.010, +0.040] small gain (n 141; +13/=127/-1) | +0.015 [+0.002, +0.034] small gain (n 81; +4/=77/-0) | +0.017 [+0.000, +0.043] no evidence (n 25; +2/=23/-0) | +0.050 [+0.011, +0.096] small gain (n 35; +7/=27/-1) |
| F minus bm25 | +0.335 [+0.283, +0.388] beats (n 141; +92/=46/-3) | +0.338 [+0.267, +0.407] beats (n 81; +51/=29/-1) | +0.316 [+0.221, +0.408] beats (n 25; +17/=8/-0) | +0.343 [+0.230, +0.456] beats (n 35; +24/=9/-2) |
| FR minus bm25 | +0.359 [+0.307, +0.412] beats (n 141; +96/=42/-3) | +0.353 [+0.284, +0.420] beats (n 81; +54/=26/-1) | +0.332 [+0.235, +0.426] beats (n 25; +17/=8/-0) | +0.393 [+0.277, +0.509] beats (n 35; +25/=8/-2) |

Declaration recall and file F1 against `grep_baseline`:

| `decl_recall` | overall | band=unique | band=some | band=common |
| --- | --- | --- | --- | --- |
| F minus grep | -0.649 [-0.695, -0.603] worse (n 141; +0/=9/-132) | -0.587 [-0.651, -0.519] worse (n 81; +0/=9/-72) | -0.692 [-0.781, -0.603] worse (n 25; +0/=0/-25) | -0.764 [-0.831, -0.696] worse (n 35; +0/=0/-35) |
| FR minus grep | -0.616 [-0.662, -0.570] worse (n 141; +0/=10/-131) | -0.560 [-0.623, -0.496] worse (n 81; +0/=9/-72) | -0.672 [-0.765, -0.579] worse (n 25; +0/=0/-25) | -0.707 [-0.787, -0.624] worse (n 35; +0/=1/-34) |

| `file_f1` | overall | band=unique | band=some | band=common |
| --- | --- | --- | --- | --- |
| F minus grep | -0.509 [-0.553, -0.461] worse (n 141; +12/=0/-129) | -0.648 [-0.681, -0.613] worse (n 81; +0/=0/-81) | -0.530 [-0.590, -0.472] worse (n 25; +0/=0/-25) | -0.171 [-0.266, -0.076] worse (n 35; +12/=0/-23) |
| FR minus grep | -0.488 [-0.535, -0.438] worse (n 141; +11/=0/-130) | -0.638 [-0.671, -0.603] worse (n 81; +0/=0/-81) | -0.510 [-0.572, -0.450] worse (n 25; +0/=0/-25) | -0.127 [-0.222, -0.030] worse (n 35; +11/=0/-24) |

Reading (rules 2 to 6):

* **F and FR against L, `decl_f1`** (rule 4, primary cell `unique` or `common`): F minus L is a small loss on
  `unique` and no evidence of a difference elsewhere; FR minus L is a small gain on `common` (+0.044, interval above
  0, below the floor) and no evidence of a difference on `unique` and `some`. **Neither F nor FR beats L on `impact`**.
* **Against `grep_baseline`** (rule 5): F and FR are worse than the baseline on `decl_f1`, `decl_recall` and
  `file_f1` overall and in every band (every interval excludes 0). The gap is smallest on `common`
  (FR 0.153 against 0.421, a difference of -0.268) and largest on `unique` (-0.82). **Neither beats the baseline.**
* **Against BM25** (file level): F and FR beat BM25 on `file_f1`, `file_recall` overall and in each band; on `file_precision` FR beats it in
  every band and F is a small gain (beats on `some`).
* **The reranker** (rule 6): FR minus F on the primary metric `decl_f1` is a small gain in every band (+0.015
  `unique`, +0.036 `common`, interval above 0, below the floor; items improved/tied/worsened 11/70/0 and 15/20/0),
  so by rule 6 there is **no evidence of an effect** large enough to call (small gain = an effect below the floor, not called). On the ranking metrics it meets rule 2 in
  `common` (`decl_recall@5` +0.060, `decl_recall@10` +0.061, `decl_ndcg@10` +0.092) and in `unique` for
  `decl_recall@10` (+0.058); window-level `decl_recall` also meets rule 2 in `common` (+0.057) with 0 items worse.
  Since the same shortlist feeds both and window recall moved too, part of the gain comes from a changed selection
  among the 24 candidates, not only from reordering [I]. No item's `decl_f1` is worse under FR than under F.
* **Fusion** (rule 7, descriptive): F is not at least as good as the better channel on `unique` (L 0.183, S 0.124,
  F 0.164, a small loss against L); on `common` (L 0.109, S 0.118, F 0.117) and `some` (L 0.115, S 0.098, F 0.124) F is within
  noise of the better one. File recall is the clearest fusion effect: F 0.876 against L 0.790 and S 0.829.
* **Product baseline for M1.4** (descriptive): `tests_deep_recall` is 0.000 for L, F and FR (n_defined 103), and
  `r2_decl_recall` is 0.18 / 0.19 / 0.21 (L / F / FR, overall, n_defined 141); M1.4 reads the `tests_deep` stratum
  against this baseline.
* **C1 caveat** (rule 6 of M0.3): file-level metrics move more than declaration-level ones. FR's `file_recall`
  (0.900 [0.866, 0.931]) is 0.110 above L's, while `decl_recall` is 0.018 above (no evidence of a difference).

## 3. `where-to` (59 items; gold accepted at its section 12.12)

Registration metrics are defined on the 25 registration items, set metrics on the 34 exemplar and convention items.
Wilson for `reg_hit@3`; bootstrap otherwise. L and BM25 are M0.3's numbers.

| arm | reg_hit@3, registration (25) | reg_mrr (25) | reg_decl_hit@3 (25) | set_recall, convention (14) | set_recall, exemplar (20) | set_precision, convention (14) | set_precision, exemplar (20) |
| --- | --- | --- | --- | --- | --- | --- | --- |
| L | 0.720 [0.524, 0.857] | 0.666 [0.491, 0.826] | 0.360 [0.203, 0.555] | 0.010 [0.000, 0.031] | 0.083 [0.006, 0.180] | 0.007 [0.000, 0.021] | 0.086 [0.006, 0.194] |
| S | 0.880 [0.700, 0.958] | 0.781 [0.641, 0.901] | 0.440 [0.267, 0.629] | 0.363 [0.155, 0.588] | 0.157 [0.055, 0.274] | 0.222 [0.094, 0.364] | 0.163 [0.058, 0.284] |
| F | 0.880 [0.700, 0.958] | 0.739 [0.598, 0.871] | 0.600 [0.407, 0.766] | 0.277 [0.128, 0.447] | 0.104 [0.025, 0.198] | 0.157 [0.071, 0.257] | 0.100 [0.021, 0.185] |
| FR | 0.920 [0.750, 0.978] | 0.837 [0.710, 0.943] | 0.680 [0.484, 0.828] | 0.389 [0.184, 0.603] | 0.162 [0.061, 0.274] | 0.234 [0.107, 0.372] | 0.165 [0.062, 0.280] |
| BM25 | 0.720 [0.524, 0.857] | 0.578 [0.427, 0.727] | 0.000 [0.000, 0.133] | 0.253 [0.084, 0.441] | 0.065 [0.017, 0.125] | 0.168 [0.059, 0.288] | 0.043 [0.010, 0.080] |

Leak strata kept apart (rule 5 of M0.3; the `clean` cell has n = 8, **too small to read**, the `stem-leak` cell
n = 17; stem-leak items are never offered as evidence of retrieval ability):

| arm | reg_hit@3 clean (8) | reg_hit@3 stem-leak (17) | reg_mrr clean | reg_mrr stem-leak |
| --- | --- | --- | --- | --- |
| L | 0.750 [0.409, 0.928] | 0.706 [0.469, 0.867] | 0.750 [0.409, 0.928] | 0.626 [0.420, 0.824] |
| S | 0.875 [0.529, 0.978] | 0.882 [0.657, 0.967] | 0.838 [0.613, 1.000] | 0.755 [0.578, 0.912] |
| F | 0.875 [0.529, 0.978] | 0.882 [0.657, 0.967] | 0.830 [0.598, 1.000] | 0.696 [0.520, 0.853] |
| FR | 1.000 [0.676, 1.000] | 0.882 [0.657, 0.967] | 0.917 [0.750, 1.000] | 0.799 [0.632, 0.941] |
| BM25 | 0.750 [0.409, 0.928] | 0.706 [0.469, 0.867] | 0.531 [0.312, 0.750] | 0.600 [0.404, 0.788] |

Paired differences on registration items (G and L are the McNemar counts of items gained and lost, for 0/1 metrics):

| `reg_hit@3` | form=registration |
| --- | --- |
| F minus L | +0.160 [+0.040, +0.320] beats (n 25; +4/=21/-0 G4/L0) |
| FR minus L | +0.200 [+0.040, +0.360] beats (n 25; +5/=20/-0 G5/L0) |
| FR minus F | +0.040 [-0.080, +0.160] no evidence (n 25; +2/=22/-1 G2/L1) |
| S minus L | +0.160 [+0.040, +0.320] beats (n 25; +4/=21/-0 G4/L0) |
| S minus F | +0.000 [-0.120, +0.120] no evidence (n 25; +1/=23/-1 G1/L1) |
| F minus bm25 | +0.160 [+0.000, +0.360] no evidence (n 25; +5/=19/-1 G5/L1) |
| FR minus bm25 | +0.200 [+0.040, +0.360] beats (n 25; +5/=20/-0 G5/L0) |

| `reg_mrr` | form=registration |
| --- | --- |
| F minus L | +0.073 [-0.057, +0.220] no evidence (n 25; +6/=16/-3) |
| FR minus L | +0.171 [+0.013, +0.341] beats (n 25; +8/=15/-2) |
| FR minus F | +0.098 [-0.021, +0.223] no evidence (n 25; +6/=17/-2) |
| S minus L | +0.116 [-0.043, +0.288] no evidence (n 25; +7/=14/-4) |
| S minus F | +0.042 [-0.047, +0.133] no evidence (n 25; +5/=17/-3) |
| F minus bm25 | +0.161 [-0.016, +0.331] no evidence (n 25; +12/=10/-3) |
| FR minus bm25 | +0.259 [+0.143, +0.380] beats (n 25; +13/=12/-0) |

| `reg_decl_hit@3` | form=registration |
| --- | --- |
| F minus L | +0.240 [+0.080, +0.400] beats (n 25; +6/=19/-0 G6/L0) |
| FR minus L | +0.320 [+0.160, +0.520] beats (n 25; +8/=17/-0 G8/L0) |
| FR minus F | +0.080 [-0.080, +0.240] no evidence (n 25; +3/=21/-1 G3/L1) |
| S minus L | +0.080 [-0.120, +0.280] no evidence (n 25; +5/=17/-3 G5/L3) |
| S minus F | -0.160 [-0.320, -0.040] worse (n 25; +0/=21/-4 G0/L4) |
| F minus bm25 | +0.600 [+0.400, +0.800] beats (n 25; +15/=10/-0 G15/L0) |
| FR minus bm25 | +0.680 [+0.480, +0.840] beats (n 25; +17/=8/-0 G17/L0) |

Set metrics on convention (n = 14) and exemplar (n = 20) items:

| `set_recall` | form=convention | form=exemplar |
| --- | --- | --- |
| F minus L | +0.267 [+0.116, +0.439] beats (n 14; +8/=6/-0) | +0.021 [-0.013, +0.059] no evidence (n 20; +3/=16/-1) |
| FR minus L | +0.378 [+0.179, +0.592] beats (n 14; +8/=6/-0) | +0.079 [+0.009, +0.161] beats (n 20; +5/=14/-1) |
| FR minus F | +0.111 [+0.020, +0.222] beats (n 14; +4/=10/-0) | +0.058 [+0.012, +0.113] beats (n 20; +5/=15/-0) |
| S minus L | +0.353 [+0.150, +0.567] beats (n 14; +7/=7/-0) | +0.074 [+0.014, +0.144] beats (n 20; +4/=16/-0) |
| S minus F | +0.086 [-0.036, +0.228] no evidence (n 14; +4/=8/-2) | +0.053 [+0.014, +0.098] beats (n 20; +5/=15/-0) |
| F minus bm25 | +0.024 [-0.110, +0.163] no evidence (n 14; +5/=5/-4) | +0.039 [+0.000, +0.086] small gain (n 20; +4/=15/-1) |
| FR minus bm25 | +0.135 [-0.045, +0.334] no evidence (n 14; +6/=5/-3) | +0.097 [+0.031, +0.173] beats (n 20; +6/=14/-0) |

| `set_precision` | form=convention | form=exemplar |
| --- | --- | --- |
| F minus L | +0.150 [+0.064, +0.250] beats (n 14; +8/=6/-0) | +0.014 [-0.043, +0.068] no evidence (n 20; +3/=15/-2) |
| FR minus L | +0.227 [+0.102, +0.358] beats (n 14; +8/=6/-0) | +0.079 [-0.010, +0.171] no evidence (n 20; +5/=14/-1) |
| FR minus F | +0.077 [+0.014, +0.157] beats (n 14; +5/=9/-0) | +0.066 [+0.017, +0.122] beats (n 20; +6/=14/-0) |
| S minus L | +0.215 [+0.093, +0.344] beats (n 14; +7/=7/-0) | +0.077 [+0.006, +0.160] beats (n 20; +4/=15/-1) |
| S minus F | +0.065 [-0.006, +0.157] no evidence (n 14; +5/=7/-2) | +0.063 [+0.021, +0.111] beats (n 20; +6/=14/-0) |
| F minus bm25 | -0.010 [-0.101, +0.078] no evidence (n 14; +5/=5/-4) | +0.057 [+0.009, +0.110] beats (n 20; +5/=14/-1) |
| FR minus bm25 | +0.067 [-0.056, +0.197] no evidence (n 14; +6/=5/-3) | +0.122 [+0.043, +0.210] beats (n 20; +7/=13/-0) |

Reading (rules 2 to 6):

* **F and FR against L** (rule 4, primary cell `reg_hit@3` or convention `set_recall`): **F beats L and FR beats L**.
  On `reg_hit@3` F is +0.160 (4 items gained, 0 lost) and FR +0.200 (5 gained, 0 lost); the interval of both
  excludes 0 and both points exceed the floor of 0.12. On convention `set_recall` F is +0.267 and FR +0.378, from a
  base of 0.010 (8 of 14 items improved, 0 worse). FR minus L on `reg_mrr` is +0.171 (beats); F minus L +0.073 (no evidence).
  `reg_decl_hit@3` rises for both (+0.240, +0.320), but the C1 caveat applies.
* **Against BM25** (rule 5): **FR beats BM25** on `reg_hit@3` (+0.200 [+0.040, +0.360]) and `reg_mrr` (+0.259); **F
  does not** (`reg_hit@3` +0.160 with an interval whose lower end is 0.000: no evidence). On convention `set_recall` neither
  F (+0.024) nor FR (+0.135 [-0.045, +0.334]) beats BM25 (no evidence); on exemplar `set_recall` FR beats it (+0.097)
  and F is a small gain. Registration cells have n = 25 and wide intervals (`reg_hit@3` of FR spans 0.75 to 0.98).
* **The reranker** (rule 6): FR minus F on `reg_hit@3` is +0.040 (2 items gained, 1 lost) and on `reg_mrr` +0.098
  [-0.021, +0.223]: **no evidence of an effect on the registration ranking metrics.** On convention `set_recall`
  it meets rule 2 (+0.111 [+0.020, +0.222], 4 improved, 0 worse) and on exemplar `set_recall` (+0.058 [+0.012,
  +0.113], 5 improved, 0 worse), which are window-level metrics: the gain comes from a changed selection among the
  24 candidates [I]. So the reranker helps on `where-to` set metrics, not on registration ranking.
* **S** (diagnostic): S equals F on registration `reg_hit@3` (0.880 both) and meets rule 2 against F on exemplar set metrics
  (`set_recall` +0.053 [+0.014, +0.098]); it does not beat F in any primary cell (convention `set_recall`
  +0.086 [-0.036, +0.228]).

## 4. `flow` (111 items; 55 headline)

Primary metric `path_recall` on the headline items, then the not-flagged split (structure is claimed only from
not-flagged items). Intervals: cluster bootstrap by `prefix_cluster`, 10,000 resamples, seed 20261014. `cl` is the
cluster count. No reference arm exists for `flow`.

| arm | headline (55, 15 cl) | headline, not flagged (26, 10 cl) | headline, flagged (29, 13 cl) | all (111, 39 cl) | non-headline (56, 37 cl) | descriptive (30, 11 cl) | named (43, 17 cl) | small-helper (16, 9 cl) | config-flow (15, 15 cl) |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| L | 0.104 [0.054, 0.197] | 0.027 [0.000, 0.100] | 0.174 [0.094, 0.309] | 0.157 [0.099, 0.235] | 0.209 [0.129, 0.291] | 0.023 [0.000, 0.086] | 0.199 [0.122, 0.322] | 0.271 [0.118, 0.405] | 0.193 [0.093, 0.302] |
| S | 0.069 [0.028, 0.138] | 0.022 [0.000, 0.050] | 0.110 [0.041, 0.220] | 0.107 [0.062, 0.169] | 0.145 [0.085, 0.209] | 0.031 [0.000, 0.070] | 0.153 [0.084, 0.272] | 0.125 [0.033, 0.212] | 0.160 [0.065, 0.265] |
| F | 0.081 [0.038, 0.158] | 0.019 [0.000, 0.083] | 0.137 [0.067, 0.235] | 0.138 [0.087, 0.206] | 0.195 [0.129, 0.271] | 0.028 [0.000, 0.100] | 0.166 [0.100, 0.266] | 0.156 [0.044, 0.286] | 0.260 [0.150, 0.367] |
| FR | 0.117 [0.060, 0.217] | 0.040 [0.000, 0.105] | 0.186 [0.094, 0.321] | 0.179 [0.120, 0.259] | 0.241 [0.164, 0.325] | 0.057 [0.022, 0.121] | 0.218 [0.131, 0.352] | 0.198 [0.083, 0.314] | 0.309 [0.180, 0.443] |

Spurious answers per item (context only, no paired verdict is read):

| arm | headline | headline, not flagged | all |
| --- | --- | --- | --- |
| L | 3.509 | 3.615 | 3.919 |
| S | 3.964 | 5.000 | 3.568 |
| F | 3.782 | 4.615 | 3.820 |
| FR | 3.836 | 4.808 | 3.459 |

Share of items whose every gold intermediate is named (Wilson), and answers per item:

| arm | headline (55) | non-headline (56) | all (111) | answers per item |
| --- | --- | --- | --- | --- |
| L | 0 of 55: 0.000 [0.000, 0.065] | 3 of 56: 0.054 [0.018, 0.146] | 3 of 111: 0.027 [0.009, 0.076] | 5.10 |
| S | 0 of 55: 0.000 [0.000, 0.065] | 1 of 56: 0.018 [0.003, 0.094] | 1 of 111: 0.009 [0.002, 0.049] | 5.06 |
| F | 0 of 55: 0.000 [0.000, 0.065] | 1 of 56: 0.018 [0.003, 0.094] | 1 of 111: 0.009 [0.002, 0.049] | 5.31 |
| FR | 0 of 55: 0.000 [0.000, 0.065] | 2 of 56: 0.036 [0.010, 0.121] | 2 of 111: 0.018 [0.005, 0.063] | 5.38 |

Paired differences in `path_recall` (cluster bootstrap of per-cluster resamples; `cl` printed where the cell is
clustered):

| `path_recall` | headline=true | headline=true|not-flagged | headline=true|flagged | overall | headline=false | stratum=descriptive | stratum=named |
| --- | --- | --- | --- | --- | --- | --- | --- |
| F minus L | -0.023 [-0.074, +0.012] no evidence (n 55; +1/=49/-5) | -0.008 [-0.032, +0.000] no evidence (n 26; +0/=25/-1) | -0.037 [-0.132, +0.022] no evidence (n 29; +1/=24/-4) | -0.019 [-0.054, +0.010] no evidence (n 111; +8/=91/-12) | -0.015 [-0.068, +0.038] no evidence (n 56; +7/=42/-7) | +0.004 [-0.021, +0.043] no evidence (n 30; +1/=28/-1) | -0.033 [-0.097, +0.005] no evidence (n 43; +2/=35/-6) |
| FR minus L | +0.012 [-0.023, +0.044] no evidence (n 55; +4/=49/-2) | +0.013 [+0.000, +0.026] no evidence (n 26; +1/=25/-0) | +0.011 [-0.057, +0.065] no evidence (n 29; +3/=24/-2) | +0.022 [-0.014, +0.057] no evidence (n 111; +17/=86/-8) | +0.031 [-0.031, +0.098] no evidence (n 56; +13/=37/-6) | +0.033 [+0.000, +0.064] no evidence (n 30; +3/=27/-0) | +0.019 [-0.025, +0.065] no evidence (n 43; +5/=35/-3) |
| FR minus F | +0.035 [+0.002, +0.085] small gain (n 55; +6/=48/-1) | +0.021 [+0.000, +0.038] no evidence (n 26; +2/=24/-0) | +0.049 [-0.007, +0.135] no evidence (n 29; +4/=24/-1) | +0.041 [+0.019, +0.068] small gain (n 111; +14/=96/-1) | +0.046 [+0.019, +0.077] small gain (n 56; +8/=48/-0) | +0.029 [+0.000, +0.045] no evidence (n 30; +3/=27/-0) | +0.052 [+0.009, +0.125] beats (n 43; +6/=36/-1) |
| S minus L | -0.036 [-0.091, +0.001] no evidence (n 55; +3/=44/-8) | -0.004 [-0.085, +0.037] no evidence (n 26; +2/=22/-2) | -0.064 [-0.140, -0.015] worse (n 29; +1/=22/-6) | -0.050 [-0.106, -0.006] worse (n 111; +9/=81/-21) | -0.064 [-0.142, +0.011] no evidence (n 56; +6/=37/-13) | +0.007 [-0.060, +0.053] no evidence (n 30; +3/=25/-2) | -0.045 [-0.101, +0.004] no evidence (n 43; +3/=31/-9) |
| S minus F | -0.013 [-0.066, +0.032] no evidence (n 55; +5/=44/-6) | +0.003 [-0.071, +0.042] no evidence (n 26; +2/=23/-1) | -0.027 [-0.092, +0.052] no evidence (n 29; +3/=21/-5) | -0.031 [-0.075, +0.005] no evidence (n 111; +8/=87/-16) | -0.049 [-0.107, +0.002] no evidence (n 56; +3/=43/-10) | +0.003 [-0.062, +0.035] no evidence (n 30; +2/=27/-1) | -0.012 [-0.054, +0.051] no evidence (n 43; +4/=33/-6) |

Reading (rules 1, 2, 4, 6):

* **Neither F nor FR beats L on `flow`.** On the headline items F minus L is -0.023 [-0.074, +0.012] and FR minus L
  +0.012 [-0.023, +0.044]: no evidence of a difference. On the headline not-flagged items, the ones that can speak
  to structure, FR minus L is +0.013 [+0.000, +0.026] (no evidence; every arm stays at or below 0.04). There is no
  gain to call a name-matching gain.
* **The descriptive items** (L 0.023): F 0.028, FR 0.057 [0.022, 0.121]; FR minus L +0.033 [+0.000, +0.064]: no
  evidence at rule 2.
* **The reranker:** FR minus F on the headline items is a small gain (+0.035 [+0.002, +0.085], 6 items improved, 1
  worse), and on the not-flagged headline items no evidence (+0.021 [+0.000, +0.038]). Rule 6: **no evidence of an effect**
  in the primary cell (small gain = an effect below the floor, not called). On `named` items it meets rule 2 (+0.052 [+0.009, +0.125]), a non-primary cell.
* No arm names a whole path on a headline item (0 of 55 each). `tau` and `exact_order` are not read.
* S is worse than L overall (-0.050 [-0.106, -0.006], exactly at the floor) and on flagged headline items (-0.064
  [-0.140, -0.015]); no evidence elsewhere.

## 5. Verdicts by pre-registered rule

| question | verdict | rule and cell |
| --- | --- | --- |
| F beats L, `impact` | no | rule 4, `decl_f1`: F minus L small loss on `unique`, no evidence on `some` and `common` |
| FR beats L, `impact` | no | rule 4: small gain on `common` (+0.044), no evidence on `unique` |
| F or FR beats `grep_baseline`, `impact` | no, worse in every band | rule 5, `decl_f1` |
| F or FR beats BM25, `impact` file level | yes, descriptive file-level comparison; not a rule-5 verdict | rule 3 names only `decl_f1` for `impact` and BM25 has no declaration metrics; `file_f1` and `file_recall` are higher in every band |
| F beats L, `where-to` | yes | rule 4, `reg_hit@3` +0.160 and convention `set_recall` +0.267 |
| FR beats L, `where-to` | yes | rule 4, `reg_hit@3` +0.200, `reg_mrr` +0.171, convention `set_recall` +0.378 |
| F beats BM25, `where-to` | no | rule 5, `reg_hit@3` no evidence, convention `set_recall` no evidence |
| FR beats BM25, `where-to` | yes on registration, no on convention | rule 5, `reg_hit@3` +0.200, `reg_mrr` +0.259; convention `set_recall` no evidence |
| F or FR beats L, `flow` | no | rule 4, headline `path_recall`, overall and not-flagged |
| Reranker helps, `impact` | no (small gains) | rule 6, `decl_f1`; ranking metrics and window recall meet rule 2 in `common` |
| Reranker helps, `where-to` | yes (convention `set_recall`, n 14: FR minus F +0.111 [+0.020, +0.222], 4/10/0); no evidence on registration ranking; a changed selection [I] | rule 6 |
| Reranker helps, `flow` | no evidence of an effect | rule 6, headline |
| S beats F in a primary cell | no | prereg section 9 |

## 6. Pre-registered expectations (section 6 of the pre-registration)

* `impact`: F and FR far below `grep_baseline` on `unique`, `decl_f1` not above 0.30, smallest gap on `common`, file
  recall at least L's 0.79. **Held** (F 0.164, FR 0.178; gap -0.30 and -0.27 on `common`; file recall 0.876 and 0.900).
* `where-to`: FR `reg_hit@3` 0.80 or more; convention `set_recall` of F and FR above L and not clearly above BM25.
  **Held** (0.920; 0.277 and 0.389 against L 0.010; both against BM25 no evidence of a difference).
* `flow`: headline `path_recall` of F and FR above L's 0.104, the largest gain on `descriptive`, little on
  not-flagged headline. **Partly failed**: F 0.081 is below L (FR 0.117 is above, within noise); FR's point gain over L
  is largest on `descriptive` (+0.033) and small on not-flagged headline (+0.013).
* The reranker improves ranking metrics a little and leaves window recall unchanged within noise. **Partly failed**:
  `decl_recall@5/10` and `reg_mrr` rise (a small gain, a gain, and no evidence), but window-level recall also rises
  in `impact` `common` (+0.057) and in `where-to` set recall.

## 7. Exploratory side record: the earlier iGPU-era run (not M0b's result)

Before amendment 12 a run was made on the earlier services (embedder `qwen3-embedding-0.6b`, vLLM, dimension 1024,
an iGPU; reranker ROCm llama.cpp `b1342`, never called). It completed the index (2,085 chunks, 1,756 embedded, 172 s),
the L identity check (0 of 311 lists differ from M0.3), S and F, and was stopped before FR. Same binary, same
tree, same gold, same converter and scorers. No verdict is read from it and it carries no weight in section 5. Single-arm
numbers (same method as above):

| arm | `impact` `decl_f1` all / unique / some / common | `impact` `file_recall` | `where-to` `reg_hit@3` / `reg_mrr` | convention / exemplar `set_recall` | `flow` headline `path_recall` / not flagged |
| --- | --- | --- | --- | --- | --- |
| S (0.6B) | 0.110 / 0.116 / 0.096 / 0.105 | 0.790 | 0.800 / 0.728 | 0.263 / 0.157 | 0.040 / 0.008 |
| F (0.6B) | 0.146 / 0.162 / 0.115 / 0.131 | 0.858 | 0.880 / 0.700 | 0.245 / 0.126 | 0.059 / 0.027 |
| F (4B, section 2 to 4) | 0.145 / 0.164 / 0.124 / 0.117 | 0.876 | 0.880 / 0.739 | 0.277 / 0.104 | 0.081 / 0.019 |
| S (4B) | 0.118 / 0.124 / 0.098 / 0.118 | 0.829 | 0.880 / 0.781 | 0.363 / 0.157 | 0.069 / 0.022 |

Intervals are in the scratch files; at these sample sizes the two embedders are indistinguishable by eye on most
columns [I]. The iGPU-era query embedding took 0.6 to 1.3 s per query at the median (per-query total up to 23.7 s),
against 0.03 to 0.18 s on the 5090.

## 8. Deviations and disclosure

* **Services changed before the run that counts.** The first attempt used the iGPU services; it was stopped when the
  A/B baseline moved to the 5090 and a 4B embedder (the pre-registration's own rule: the A/B's value wins). The
  amendment (section 12) was committed before any query of the run reported here; the earlier attempt is section 7.
  An earlier proposal (section 11: a second-GPU reranker on another port) was superseded by the port move and never run.
* **No exploratory score read before amendment 12.** No exploratory 0.6B score was read before amendment 12
  (13:10:38Z): the S run file and its score existed from about 11:41Z, but neither the run agent nor the coordinator
  read any score; the trigger was the owner's decision to move every model to the RTX 5090 (A/B addendum 10.6), not a
  result.
* **Overwritten iGPU score files.** The stats scripts overwrote the iGPU S and F score/stats files when the 5090 arms
  were scored; the unchanged run files (byte-identical in `out/` and `out_igpu/`, sha256 checked) were re-scored
  deterministically into `out_igpu/` with 0 mismatches.
* **300 s wall-clock rule.** `lci evaluate` has no per-query wall-clock limit of its own; the rule was checked from the
  per-query total time in the report (maximum 1,788 ms) together with the product's own 240 s reranker timeout.
  No process-level watchdog was used. No query came near the limit.
* **Retry pass:** empty (0 degraded queries).
* **Exit code.** `lci evaluate` exits 1 when expected paths are missing (6 FR queries here, 3 + 1 + 2; 8, 8 and 23 for L in M0.3); the reports
  were complete and the scorers do not use the expected paths. A report outcome of "failure" with reason "no
  expected-path hit" is not a search failure, and degraded-query detection tested for `reranked`, a warning, a search
  error and the 300 s limit, not for that outcome.
* **Wilson versus bootstrap** applied mechanically, as in M0.3 (some small-cell recall@k values carry Wilson
  intervals).
* **Paired spurious counts** were computed but their "beats/worse" labels have no meaning (more spurious is worse); no
  verdict is read from them.
* **Load.** Shared services (section 1); timings are context, not latency evidence. Run-to-run variation of the
  reranker was not measured (one run).
* **Intervals** use Python's `random.Random(20261014)` with `choices` for paired bootstraps and the M0.3 per-item
  bootstrap for the single-arm tables; the same method, a different resampling call than M0.3's paired script.
* **Cells under 10 items** are labelled too small to read where read at all: `impact` coverage `tests-far` (8),
  `where-to` `leak=clean` (8), `flow` strata `dispatch` (5) and `multi-path` (2).
* **Gold state** as in M0.3 section 2: `impact` v2 with a fresh independent audit still owed; `where-to` accepted at
  its section 12.12; `flow` v2 audited, descriptive wording unread by a second person.
* Nothing else differs from the pre-registration. No setting was changed after the first FR query.

## 9. What this changes (prereg section 9; nothing here is a gate)

* [I] The model channels help the locating questions: on `where-to`, FR beats L on registration and convention and
  beats BM25 on registration; on `impact` they raise file recall (0.79 to 0.90) and the ranking of declarations but
  `decl_f1` stays far below `grep_baseline` and not above L; on `flow` they do not move headline `path_recall`
  beyond noise. The outcome matches none of the four listed cases exactly: it is closest to "FR beats L on
  `where-to` but not on `impact` or `flow`".
* [I] The multimap work can take FR as the retrieval base for locating (`where-to`) questions; `impact` and `flow`
  remain for structural maps, since neither channel nor reranker closes the gap to `grep_baseline` or moves the
  `flow` headline. The reranker's gain on `where-to` set metrics and on `impact` ranking is real but small; its cost
  is about 1 s per search on this machine [M, under load].
* These numbers are the "product baseline" column for M0.4 (the scored walk on `flow`) and later arms. No outcome
  supports a claim about agent performance.

Scripts, configurations, raw reports, run files and per-item files stay in the scratch directory.
