# Third-party notices

Generated file: do not edit by hand. It lists every third-party crate in `Cargo.lock` (all targets, every
platform's dependencies) with its version and the SPDX licences its manifest or licence files declare. A crate
lists several licences when it offers a choice (for example `MIT, Apache-2.0` means MIT OR Apache-2.0). The
workspace's own crates are not listed: LCI itself has no licence file on purpose.

The list is not a copy of the licence texts. Each crate's licence texts ship in its crates.io package and in the
vendored source (`cargo vendor`); a binary distribution must carry them.

Regenerate after any `Cargo.lock` change (needs [cargo-deny](https://github.com/EmbarkStudios/cargo-deny),
`cargo install cargo-deny --locked`; the job `deny` in `.github/workflows/ci.yml` checks the same licence set):

```bash
{ sed '/^```text$/q' THIRD_PARTY_NOTICES.md; cargo deny --color never list -l crate | grep -v ' (0): *$'; echo '```'; } > notices.tmp && mv notices.tmp THIRD_PARTY_NOTICES.md
```

Last generated from the `Cargo.lock` of the commit that introduced this file, with cargo-deny 0.20.2.

```text
adler2@2.0.1 (3): 0BSD, MIT, Apache-2.0
ahash@0.8.12 (2): MIT, Apache-2.0
aho-corasick@1.1.5 (2): Unlicense, MIT
allocator-api2@0.2.21 (2): MIT, Apache-2.0
android_system_properties@0.1.6 (2): MIT, Apache-2.0
anstream@1.0.0 (2): MIT, Apache-2.0
anstyle@1.0.14 (2): MIT, Apache-2.0
anstyle-parse@1.0.0 (2): MIT, Apache-2.0
anstyle-query@1.1.5 (2): MIT, Apache-2.0
anstyle-wincon@3.0.11 (2): MIT, Apache-2.0
anyhow@1.0.104 (2): MIT, Apache-2.0
arc-swap@1.9.2 (2): MIT, Apache-2.0
arrayref@0.3.9 (1): BSD-2-Clause
arrayvec@0.7.8 (2): MIT, Apache-2.0
arrow@58.4.0 (1): Apache-2.0
arrow-arith@58.4.0 (1): Apache-2.0
arrow-array@58.4.0 (2): Apache-2.0, MIT
arrow-buffer@58.4.0 (1): Apache-2.0
arrow-cast@58.4.0 (1): Apache-2.0
arrow-csv@58.4.0 (1): Apache-2.0
arrow-data@58.4.0 (1): Apache-2.0
arrow-ipc@58.4.0 (1): Apache-2.0
arrow-json@58.4.0 (1): Apache-2.0
arrow-ord@58.4.0 (1): Apache-2.0
arrow-row@58.4.0 (1): Apache-2.0
arrow-schema@58.4.0 (1): Apache-2.0
arrow-select@58.4.0 (1): Apache-2.0
arrow-string@58.4.0 (1): Apache-2.0
async-channel@2.5.0 (2): Apache-2.0, MIT
async-compression@0.4.46 (2): MIT, Apache-2.0
async-lock@3.4.2 (2): Apache-2.0, MIT
async-recursion@1.1.1 (2): MIT, Apache-2.0
async-trait@0.1.92 (2): MIT, Apache-2.0
async_cell@0.2.3 (1): MIT
atoi@2.0.0 (1): MIT
atomic-waker@1.1.2 (2): Apache-2.0, MIT
autocfg@1.5.1 (2): Apache-2.0, MIT
aws-lc-rs@1.18.1 (2): ISC, Apache-2.0
aws-lc-sys@0.45.0 (5): ISC, Apache-2.0, MIT, BSD-3-Clause, MIT-0
axum@0.7.9 (1): MIT
axum@0.8.9 (1): MIT
axum-core@0.4.5 (1): MIT
axum-core@0.5.6 (1): MIT
base64@0.22.1 (2): MIT, Apache-2.0
base64@0.23.1 (2): MIT, Apache-2.0
bigdecimal@0.4.10 (2): MIT, Apache-2.0
bitflags@1.3.2 (2): MIT, Apache-2.0
bitflags@2.13.1 (2): MIT, Apache-2.0
bitvec@1.1.1 (1): MIT
blake2@0.10.6 (2): MIT, Apache-2.0
blake3@1.8.7 (3): CC0-1.0, Apache-2.0, Apache-2.0 WITH LLVM-exception
block-buffer@0.10.4 (2): MIT, Apache-2.0
block-buffer@0.12.1 (2): MIT, Apache-2.0
bstr@1.13.1 (2): MIT, Apache-2.0
bumpalo@3.20.3 (2): MIT, Apache-2.0
bytecheck@0.8.3 (1): MIT
bytecheck_derive@0.8.3 (1): MIT
bytecount@0.6.9 (2): Apache-2.0, MIT
bytemuck@1.25.2 (3): Zlib, Apache-2.0, MIT
bytemuck_derive@1.12.0 (3): Zlib, Apache-2.0, MIT
byteorder@1.5.0 (2): Unlicense, MIT
bytes@1.12.1 (1): MIT
cc@1.4.5 (2): MIT, Apache-2.0
cedarwood@0.5.0 (1): BSD-2-Clause
cfg-if@1.0.4 (2): MIT, Apache-2.0
chacha20@0.10.2 (2): MIT, Apache-2.0
chrono@0.4.45 (2): MIT, Apache-2.0
chrono-tz@0.10.4 (2): MIT, Apache-2.0
clap@4.6.7 (2): MIT, Apache-2.0
clap_builder@4.6.7 (2): MIT, Apache-2.0
clap_derive@4.6.7 (2): MIT, Apache-2.0
clap_lex@1.1.0 (2): MIT, Apache-2.0
cmake@0.1.58 (2): MIT, Apache-2.0
colorchoice@1.0.5 (2): MIT, Apache-2.0
combine@4.6.8 (1): MIT
comfy-table@7.2.2 (1): MIT
compression-codecs@0.4.41 (2): MIT, Apache-2.0
compression-core@0.4.33 (2): MIT, Apache-2.0
concurrent-queue@2.5.0 (2): Apache-2.0, MIT
const-oid@0.10.2 (2): Apache-2.0, MIT
const-random@0.1.18 (2): MIT, Apache-2.0
const-random-macro@0.1.16 (2): MIT, Apache-2.0
constant_time_eq@0.4.2 (3): CC0-1.0, MIT-0, Apache-2.0
core-foundation@0.10.1 (2): MIT, Apache-2.0
core-foundation@0.9.4 (2): MIT, Apache-2.0
core-foundation-sys@0.8.7 (2): MIT, Apache-2.0
core_detect@1.0.0 (2): MIT, Apache-2.0
cpufeatures@0.2.17 (2): MIT, Apache-2.0
cpufeatures@0.3.1 (2): MIT, Apache-2.0
crc32fast@1.5.1 (2): MIT, Apache-2.0
crossbeam-channel@0.5.17 (2): MIT, Apache-2.0
crossbeam-deque@0.8.8 (2): MIT, Apache-2.0
crossbeam-epoch@0.9.21 (2): MIT, Apache-2.0
crossbeam-queue@0.3.14 (2): MIT, Apache-2.0
crossbeam-skiplist@0.1.3 (2): MIT, Apache-2.0
crossbeam-utils@0.8.23 (2): MIT, Apache-2.0
crunchy@0.2.4 (1): MIT
crypto-common@0.1.7 (2): MIT, Apache-2.0
crypto-common@0.2.2 (2): MIT, Apache-2.0
csv@1.4.0 (2): Unlicense, MIT
csv-core@0.1.13 (2): Unlicense, MIT
daachorse@2.1.1 (2): MIT, Apache-2.0
darling@0.20.11 (1): MIT
darling@0.24.1 (1): MIT
darling_core@0.20.11 (1): MIT
darling_core@0.24.1 (1): MIT
darling_macro@0.20.11 (1): MIT
darling_macro@0.24.1 (1): MIT
dashmap@6.2.1 (1): MIT
datafusion@54.1.0 (1): Apache-2.0
datafusion-catalog@54.1.0 (1): Apache-2.0
datafusion-catalog-listing@54.1.0 (1): Apache-2.0
datafusion-common@54.1.0 (1): Apache-2.0
datafusion-common-runtime@54.1.0 (1): Apache-2.0
datafusion-datasource@54.1.0 (1): Apache-2.0
datafusion-datasource-arrow@54.1.0 (1): Apache-2.0
datafusion-datasource-csv@54.1.0 (1): Apache-2.0
datafusion-datasource-json@54.1.0 (1): Apache-2.0
datafusion-doc@54.1.0 (1): Apache-2.0
datafusion-execution@54.1.0 (1): Apache-2.0
datafusion-expr@54.1.0 (1): Apache-2.0
datafusion-expr-common@54.1.0 (1): Apache-2.0
datafusion-functions@54.1.0 (1): Apache-2.0
datafusion-functions-aggregate@54.1.0 (1): Apache-2.0
datafusion-functions-aggregate-common@54.1.0 (1): Apache-2.0
datafusion-functions-nested@54.1.0 (1): Apache-2.0
datafusion-functions-table@54.1.0 (1): Apache-2.0
datafusion-functions-window@54.1.0 (1): Apache-2.0
datafusion-functions-window-common@54.1.0 (1): Apache-2.0
datafusion-macros@54.1.0 (1): Apache-2.0
datafusion-optimizer@54.1.0 (1): Apache-2.0
datafusion-physical-expr@54.1.0 (1): Apache-2.0
datafusion-physical-expr-adapter@54.1.0 (1): Apache-2.0
datafusion-physical-expr-common@54.1.0 (1): Apache-2.0
datafusion-physical-optimizer@54.1.0 (1): Apache-2.0
datafusion-physical-plan@54.1.0 (1): Apache-2.0
datafusion-pruning@54.1.0 (1): Apache-2.0
datafusion-session@54.1.0 (1): Apache-2.0
datafusion-sql@54.1.0 (1): Apache-2.0
derive_builder@0.20.2 (2): MIT, Apache-2.0
derive_builder_core@0.20.2 (2): MIT, Apache-2.0
derive_builder_macro@0.20.2 (2): MIT, Apache-2.0
digest@0.10.7 (2): MIT, Apache-2.0
digest@0.11.3 (2): MIT, Apache-2.0
dirs@6.0.0 (2): MIT, Apache-2.0
dirs-sys@0.5.0 (2): MIT, Apache-2.0
displaydoc@0.2.7 (2): MIT, Apache-2.0
dunce@1.0.5 (3): CC0-1.0, MIT-0, Apache-2.0
dyn-clone@1.0.20 (2): MIT, Apache-2.0
either@1.18.0 (2): MIT, Apache-2.0
encoding_rs@0.8.41 (3): Apache-2.0, MIT, BSD-3-Clause
encoding_rs_io@0.1.8 (2): MIT, Apache-2.0
equivalent@1.0.2 (2): Apache-2.0, MIT
errno@0.3.14 (2): MIT, Apache-2.0
ethnum@1.5.3 (2): MIT, Apache-2.0
event-listener@5.4.2 (2): Apache-2.0, MIT
event-listener-strategy@0.5.4 (2): Apache-2.0, MIT
fast-float2@0.2.4 (2): MIT, Apache-2.0
fastrand@2.5.0 (2): Apache-2.0, MIT
find-msvc-tools@0.1.12 (2): MIT, Apache-2.0
fixedbitset@0.5.7 (2): MIT, Apache-2.0
flatbuffers@25.12.19 (1): Apache-2.0
flate2@1.1.10 (2): MIT, Apache-2.0
fnv@1.0.7 (2): Apache-2.0, MIT
foldhash@0.1.5 (1): Zlib
foldhash@0.2.0 (1): Zlib
form_urlencoded@1.2.2 (2): MIT, Apache-2.0
frostem@1.20260821.5 (1): BSD-3-Clause
fs_extra@1.3.0 (1): MIT
fsevent-sys@4.1.0 (1): MIT
fsst@11.0.0 (1): Apache-2.0
fst@0.4.7 (2): Unlicense, MIT
funty@2.0.0 (1): MIT
futures@0.3.34 (2): MIT, Apache-2.0
futures-channel@0.3.34 (2): MIT, Apache-2.0
futures-core@0.3.34 (2): MIT, Apache-2.0
futures-executor@0.3.34 (2): MIT, Apache-2.0
futures-io@0.3.34 (2): MIT, Apache-2.0
futures-macro@0.3.34 (2): MIT, Apache-2.0
futures-sink@0.3.34 (2): MIT, Apache-2.0
futures-task@0.3.34 (2): MIT, Apache-2.0
futures-util@0.3.34 (2): MIT, Apache-2.0
generator@0.8.9 (2): MIT, Apache-2.0
generic-array@0.14.7 (1): MIT
getrandom@0.2.17 (2): MIT, Apache-2.0
getrandom@0.3.4 (2): MIT, Apache-2.0
getrandom@0.4.3 (2): MIT, Apache-2.0
glob@0.3.4 (2): MIT, Apache-2.0
globset@0.4.20 (2): Unlicense, MIT
h2@0.4.19 (1): MIT
half@2.7.1 (2): MIT, Apache-2.0
hashbrown@0.14.5 (2): MIT, Apache-2.0
hashbrown@0.15.5 (2): MIT, Apache-2.0
hashbrown@0.16.1 (2): MIT, Apache-2.0
hashbrown@0.17.1 (2): MIT, Apache-2.0
heck@0.5.0 (2): MIT, Apache-2.0
hermit-abi@0.5.3 (2): MIT, Apache-2.0
hex@0.4.3 (2): MIT, Apache-2.0
http@1.5.0 (2): MIT, Apache-2.0
http-body@1.1.0 (1): MIT
http-body-util@0.1.5 (1): MIT
httparse@1.10.1 (2): MIT, Apache-2.0
httpdate@1.0.3 (2): MIT, Apache-2.0
humantime@2.4.0 (2): MIT, Apache-2.0
hybrid-array@0.4.15 (2): MIT, Apache-2.0
hyper@1.11.1 (1): MIT
hyper-rustls@0.27.9 (3): Apache-2.0, ISC, MIT
hyper-util@0.1.20 (1): MIT
hyperloglogplus@0.4.1 (1): MIT
iana-time-zone@0.1.65 (2): MIT, Apache-2.0
iana-time-zone-haiku@0.1.2 (2): MIT, Apache-2.0
icu_collections@2.3.0 (1): Unicode-3.0
icu_locale_core@2.3.0 (1): Unicode-3.0
icu_locale_fallback@2.3.0 (1): Unicode-3.0
icu_locale_fallback_data@2.3.0 (1): Unicode-3.0
icu_normalizer@2.3.0 (1): Unicode-3.0
icu_normalizer_data@2.3.0 (1): Unicode-3.0
icu_properties@2.3.0 (1): Unicode-3.0
icu_properties_data@2.3.0 (1): Unicode-3.0
icu_provider@2.3.1 (1): Unicode-3.0
icu_segmenter@2.3.0 (1): Unicode-3.0
icu_segmenter_data@2.3.0 (1): Unicode-3.0
ident_case@1.0.1 (2): MIT, Apache-2.0
idna@1.1.0 (2): MIT, Apache-2.0
idna_adapter@1.2.2 (2): Apache-2.0, MIT
ignore@0.4.33 (2): Unlicense, MIT
indexmap@2.14.2 (2): Apache-2.0, MIT
inotify@0.11.5 (1): ISC
inotify-sys@0.1.8 (1): ISC
io-uring@0.7.15 (2): MIT, Apache-2.0
ipnet@2.12.2 (2): MIT, Apache-2.0
is_terminal_polyfill@1.70.2 (2): MIT, Apache-2.0
itertools@0.14.0 (2): MIT, Apache-2.0
itoa@1.0.18 (2): MIT, Apache-2.0
jieba-macros@0.10.3 (1): MIT
jieba-rs@0.10.3 (1): MIT
jiff@0.2.35 (2): Unlicense, MIT
jiff-core@0.1.0 (2): Unlicense, MIT
jiff-tzdb@0.1.8 (2): Unlicense, MIT
jiff-tzdb-platform@0.1.3 (2): Unlicense, MIT
jni@0.22.4 (2): MIT, Apache-2.0
jni-macros@0.22.4 (2): MIT, Apache-2.0
jni-sys@0.4.1 (2): MIT, Apache-2.0
jni-sys-macros@0.4.1 (2): MIT, Apache-2.0
jobserver@0.1.35 (2): MIT, Apache-2.0
js-sys@0.3.105 (2): MIT, Apache-2.0
jsonb@0.5.6 (1): Apache-2.0
kanaria@0.2.0 (1): MIT
kqueue@1.2.1 (1): MIT
kqueue-sys@1.1.2 (1): MIT
lance@11.0.0 (1): Apache-2.0
lance-arrow@11.0.0 (1): Apache-2.0
lance-arrow-scalar@58.0.0 (1): Apache-2.0
lance-arrow-stats@58.0.0 (1): Apache-2.0
lance-bitpacking@11.0.0 (1): Apache-2.0
lance-core@11.0.0 (1): Apache-2.0
lance-datafusion@11.0.0 (1): Apache-2.0
lance-datagen@11.0.0 (1): Apache-2.0
lance-derive@11.0.0 (1): Apache-2.0
lance-encoding@11.0.0 (1): Apache-2.0
lance-file@11.0.0 (1): Apache-2.0
lance-index@11.0.0 (1): Apache-2.0
lance-index-core@11.0.0 (1): Apache-2.0
lance-io@11.0.0 (1): Apache-2.0
lance-linalg@11.0.0 (1): Apache-2.0
lance-namespace@11.0.0 (1): Apache-2.0
lance-namespace-impls@11.0.0 (1): Apache-2.0
lance-namespace-reqwest-client@0.11.1 (1): Apache-2.0
lance-select@11.0.0 (1): Apache-2.0
lance-table@11.0.0 (1): Apache-2.0
lance-tokenizer@11.0.0 (1): Apache-2.0
lancedb@0.38.0 (1): Apache-2.0
lazy_static@1.5.0 (2): MIT, Apache-2.0
lexical-core@1.0.6 (2): MIT, Apache-2.0
lexical-parse-float@1.0.6 (2): MIT, Apache-2.0
lexical-parse-integer@1.0.6 (2): MIT, Apache-2.0
lexical-util@1.0.7 (2): MIT, Apache-2.0
lexical-write-float@1.0.6 (2): MIT, Apache-2.0
lexical-write-integer@1.0.6 (2): MIT, Apache-2.0
libc@0.2.189 (2): MIT, Apache-2.0
libm@0.2.16 (1): MIT
libredox@0.1.23 (1): MIT
libsais-rs@0.2.2 (1): Apache-2.0
lindera@3.0.7 (1): MIT
lindera-dictionary@3.0.7 (1): MIT
linux-raw-sys@0.12.1 (3): Apache-2.0 WITH LLVM-exception, Apache-2.0, MIT
litemap@0.8.3 (1): Unicode-3.0
lock_api@0.4.14 (2): MIT, Apache-2.0
log@0.4.34 (2): MIT, Apache-2.0
loom@0.7.2 (1): MIT
lz4@1.28.1 (1): MIT
lz4-sys@1.11.1+lz4-1.10.0 (1): MIT
lz4_flex@0.13.1 (1): MIT
matchers@0.2.0 (1): MIT
matchit@0.7.3 (2): MIT, BSD-3-Clause
matchit@0.8.4 (2): MIT, BSD-3-Clause
matrixmultiply@0.3.11 (2): MIT, Apache-2.0
md-5@0.11.0 (2): MIT, Apache-2.0
memchr@2.8.3 (2): Unlicense, MIT
memmap2@0.9.11 (2): MIT, Apache-2.0
mime@0.3.17 (2): MIT, Apache-2.0
mime_guess@2.0.5 (1): MIT
miniz_oxide@0.9.1 (3): MIT, Zlib, Apache-2.0
mio@1.2.3 (1): MIT
moka@0.12.16 (2): MIT, Apache-2.0
multimap@0.10.1 (2): MIT, Apache-2.0
multiversion@0.9.0 (2): MIT, Apache-2.0
multiversion-macros@0.9.0 (2): MIT, Apache-2.0
multiversion_no_op@1.0.0 (2): Apache-2.0, MIT
munge@0.4.7 (1): MIT
munge_macro@0.4.7 (1): MIT
ndarray@0.16.1 (2): MIT, Apache-2.0
nom@8.0.0 (1): MIT
notify@8.2.0 (1): CC0-1.0
notify-types@2.1.0 (2): MIT, Apache-2.0
nu-ansi-term@0.50.3 (1): MIT
num-bigint@0.4.8 (2): MIT, Apache-2.0
num-complex@0.4.6 (2): MIT, Apache-2.0
num-integer@0.1.47 (2): MIT, Apache-2.0
num-traits@0.2.19 (2): MIT, Apache-2.0
num_cpus@1.17.0 (2): MIT, Apache-2.0
object_store@0.13.2 (2): MIT, Apache-2.0
once_cell@1.21.4 (2): MIT, Apache-2.0
once_cell_polyfill@1.70.2 (2): MIT, Apache-2.0
openssl-probe@0.2.1 (2): MIT, Apache-2.0
option-ext@0.2.0 (1): MPL-2.0
ordered-float@5.5.0 (1): MIT
parking@2.2.1 (2): Apache-2.0, MIT
parking_lot@0.12.5 (2): MIT, Apache-2.0
parking_lot_core@0.9.12 (2): MIT, Apache-2.0
paste@1.0.15 (2): MIT, Apache-2.0
pastey@0.2.3 (2): MIT, Apache-2.0
path_abs@0.5.1 (2): MIT, Apache-2.0
percent-encoding@2.3.2 (2): MIT, Apache-2.0
permutation@0.4.1 (2): Apache-2.0, MIT
petgraph@0.8.3 (2): MIT, Apache-2.0
phf@0.12.1 (1): MIT
phf@0.13.1 (1): MIT
phf_codegen@0.13.1 (1): MIT
phf_generator@0.13.1 (1): MIT
phf_shared@0.12.1 (1): MIT
phf_shared@0.13.1 (1): MIT
pin-project@1.1.13 (2): Apache-2.0, MIT
pin-project-internal@1.1.13 (2): Apache-2.0, MIT
pin-project-lite@0.2.17 (2): Apache-2.0, MIT
pkg-config@0.3.34 (2): MIT, Apache-2.0
portable-atomic@1.15.0 (2): Apache-2.0, MIT
portable-atomic-util@0.2.8 (2): Apache-2.0, MIT
potential_utf@0.1.6 (1): Unicode-3.0
ppv-lite86@0.2.21 (2): MIT, Apache-2.0
prettyplease@0.2.37 (2): MIT, Apache-2.0
proc-macro2@1.0.107 (2): MIT, Apache-2.0
prost@0.14.4 (1): Apache-2.0
prost-build@0.14.4 (1): Apache-2.0
prost-derive@0.14.4 (1): Apache-2.0
prost-types@0.14.4 (1): Apache-2.0
protobuf@3.7.2 (1): MIT
protobuf-support@3.7.2 (1): MIT
ptr_meta@0.3.2 (1): MIT
ptr_meta_derive@0.3.2 (1): MIT
quick_cache@0.6.24 (1): MIT
quote@1.0.47 (2): MIT, Apache-2.0
r-efi@5.3.0 (3): MIT, Apache-2.0, LGPL-2.1-or-later
r-efi@6.0.0 (3): MIT, Apache-2.0, LGPL-2.1-or-later
radium@0.7.0 (1): MIT
rancor@0.1.3 (1): MIT
rand@0.10.2 (2): MIT, Apache-2.0
rand@0.9.5 (2): MIT, Apache-2.0
rand_chacha@0.9.0 (2): MIT, Apache-2.0
rand_core@0.10.1 (2): MIT, Apache-2.0
rand_core@0.9.5 (2): MIT, Apache-2.0
rand_distr@0.5.1 (2): MIT, Apache-2.0
rand_xoshiro@0.7.0 (2): MIT, Apache-2.0
rangemap@1.8.0 (2): MIT, Apache-2.0
rawpointer@0.2.1 (2): MIT, Apache-2.0
rayon@1.12.0 (2): MIT, Apache-2.0
rayon-core@1.13.0 (2): MIT, Apache-2.0
redox_syscall@0.5.18 (1): MIT
redox_users@0.5.2 (1): MIT
ref-cast@1.0.27 (2): MIT, Apache-2.0
ref-cast-impl@1.0.27 (2): MIT, Apache-2.0
regex@1.13.1 (2): MIT, Apache-2.0
regex-automata@0.4.18 (2): MIT, Apache-2.0
regex-syntax@0.8.11 (2): MIT, Apache-2.0
rend@0.5.4 (1): MIT
reqwest@0.12.28 (2): MIT, Apache-2.0
reqwest@0.13.5 (2): MIT, Apache-2.0
ring@0.17.14 (2): Apache-2.0, ISC
rkyv@0.8.18 (1): MIT
rkyv_derive@0.8.18 (1): MIT
rmcp@3.2.0 (1): Apache-2.0
rmcp-macros@3.2.0 (1): Apache-2.0
roaring@0.11.5 (2): MIT, Apache-2.0
rustc-hash@2.1.3 (2): Apache-2.0, MIT
rustc_version@0.4.1 (2): MIT, Apache-2.0
rustix@1.1.4 (3): Apache-2.0 WITH LLVM-exception, Apache-2.0, MIT
rustls@0.23.46 (3): Apache-2.0, ISC, MIT
rustls-native-certs@0.8.4 (3): Apache-2.0, ISC, MIT
rustls-pki-types@1.15.1 (2): MIT, Apache-2.0
rustls-platform-verifier@0.7.0 (2): MIT, Apache-2.0
rustls-platform-verifier-android@0.1.1 (2): MIT, Apache-2.0
rustls-webpki@0.103.15 (1): ISC
rustversion@1.0.23 (2): MIT, Apache-2.0
ryu@1.0.23 (2): Apache-2.0, BSL-1.0
same-file@1.0.6 (2): Unlicense, MIT
schannel@0.1.29 (1): MIT
schemars@1.2.2 (1): MIT
schemars_derive@1.2.2 (1): MIT
scip@0.10.0 (1): Apache-2.0
scoped-tls@1.0.1 (2): MIT, Apache-2.0
scopeguard@1.2.0 (2): MIT, Apache-2.0
security-framework@3.7.0 (2): MIT, Apache-2.0
security-framework-sys@2.17.0 (2): MIT, Apache-2.0
semver@1.0.28 (2): MIT, Apache-2.0
seq-macro@0.3.6 (2): MIT, Apache-2.0
serde@1.0.229 (2): MIT, Apache-2.0
serde_core@1.0.229 (2): MIT, Apache-2.0
serde_derive@1.0.229 (2): MIT, Apache-2.0
serde_derive_internals@0.30.0 (2): MIT, Apache-2.0
serde_json@1.0.151 (2): MIT, Apache-2.0
serde_path_to_error@0.1.20 (2): MIT, Apache-2.0
serde_repr@0.1.21 (2): MIT, Apache-2.0
serde_spanned@1.1.1 (2): MIT, Apache-2.0
serde_urlencoded@0.7.1 (2): MIT, Apache-2.0
serde_with@3.23.0 (2): MIT, Apache-2.0
serde_with_macros@3.23.0 (2): MIT, Apache-2.0
serde_yaml_ng@0.10.0 (1): MIT
sha1_smol@1.0.1 (1): BSD-3-Clause
sha2@0.10.9 (2): MIT, Apache-2.0
sha2@0.11.0 (2): MIT, Apache-2.0
sharded-slab@0.1.7 (1): MIT
shlex@2.0.1 (2): MIT, Apache-2.0
signal-hook-registry@1.4.8 (2): MIT, Apache-2.0
simd-adler32@0.3.10 (1): MIT
simd_cesu8@1.2.0 (2): Apache-2.0, MIT
simdutf8@0.1.5 (2): MIT, Apache-2.0
siphasher@1.0.3 (2): MIT, Apache-2.0
slab@0.4.12 (1): MIT
smallvec@1.16.0 (2): MIT, Apache-2.0
snafu@0.8.9 (2): MIT, Apache-2.0
snafu@0.9.2 (2): MIT, Apache-2.0
snafu-derive@0.8.9 (2): MIT, Apache-2.0
snafu-derive@0.9.2 (2): MIT, Apache-2.0
socket2@0.6.5 (2): MIT, Apache-2.0
sqlparser@0.62.0 (1): Apache-2.0
sqlparser_derive@0.5.0 (1): Apache-2.0
sse-stream@0.2.6 (2): MIT, Apache-2.0
stable_deref_trait@1.2.1 (2): MIT, Apache-2.0
std_prelude@0.2.12 (1): MIT
stfu8@0.2.7 (2): MIT, Apache-2.0
stop-words@0.10.0 (2): MIT, Apache-2.0
streaming-iterator@0.1.9 (2): MIT, Apache-2.0
strsim@0.11.1 (1): MIT
strum@0.28.0 (1): MIT
strum_macros@0.28.0 (1): MIT
subtle@2.6.1 (1): BSD-3-Clause
syn@2.0.119 (2): MIT, Apache-2.0
syn@3.0.5 (2): MIT, Apache-2.0
sync_wrapper@1.0.2 (1): Apache-2.0
synstructure@0.13.2 (1): MIT
system-configuration@0.7.0 (2): MIT, Apache-2.0
system-configuration-sys@0.6.0 (2): MIT, Apache-2.0
tagptr@0.2.0 (2): MIT, Apache-2.0
tap@1.0.1 (1): MIT
tempfile@3.27.0 (2): MIT, Apache-2.0
thiserror@1.0.69 (2): MIT, Apache-2.0
thiserror@2.0.20 (2): MIT, Apache-2.0
thiserror-impl@1.0.69 (2): MIT, Apache-2.0
thiserror-impl@2.0.20 (2): MIT, Apache-2.0
thread-tree@0.3.3 (2): MIT, Apache-2.0
thread_local@1.1.10 (2): MIT, Apache-2.0
tiny-keccak@2.0.2 (1): CC0-1.0
tinystr@0.8.4 (1): Unicode-3.0
tinyvec@1.13.2 (3): Zlib, Apache-2.0, MIT
tinyvec_macros@0.1.1 (3): MIT, Apache-2.0, Zlib
tokio@1.53.1 (1): MIT
tokio-macros@2.7.2 (1): MIT
tokio-rustls@0.26.5 (2): MIT, Apache-2.0
tokio-stream@0.1.19 (1): MIT
tokio-util@0.7.19 (1): MIT
toml@0.9.12+spec-1.1.0 (2): MIT, Apache-2.0
toml_datetime@0.7.5+spec-1.1.0 (2): MIT, Apache-2.0
toml_parser@1.1.3+spec-1.1.0 (2): MIT, Apache-2.0
toml_writer@1.1.2+spec-1.1.0 (2): MIT, Apache-2.0
tower@0.5.3 (1): MIT
tower-http@0.5.2 (1): MIT
tower-http@0.6.11 (1): MIT
tower-layer@0.3.3 (1): MIT
tower-service@0.3.3 (1): MIT
tracing@0.1.44 (1): MIT
tracing-attributes@0.1.31 (1): MIT
tracing-core@0.1.36 (1): MIT
tracing-log@0.2.0 (1): MIT
tracing-subscriber@0.3.23 (1): MIT
tree-sitter@0.25.10 (1): MIT
tree-sitter-c@0.24.2 (1): MIT
tree-sitter-c-sharp@0.23.5 (1): MIT
tree-sitter-cpp@0.23.4 (1): MIT
tree-sitter-go@0.25.0 (1): MIT
tree-sitter-java@0.23.5 (1): MIT
tree-sitter-javascript@0.23.1 (1): MIT
tree-sitter-language@0.1.8 (1): MIT
tree-sitter-python@0.23.6 (1): MIT
tree-sitter-rust@0.24.2 (1): MIT
tree-sitter-typescript@0.23.2 (1): MIT
try-lock@0.2.5 (1): MIT
twox-hash@2.1.4 (1): MIT
typenum@1.20.1 (2): MIT, Apache-2.0
unicase@2.9.0 (2): MIT, Apache-2.0
unicode-blocks@0.1.10 (1): MIT
unicode-ident@1.0.24 (3): MIT, Apache-2.0, Unicode-3.0
unicode-normalization@0.1.25 (2): MIT, Apache-2.0
unicode-segmentation@1.13.3 (2): MIT, Apache-2.0
unicode-width@0.2.2 (2): MIT, Apache-2.0
unsafe-libyaml@0.2.11 (1): MIT
untrusted@0.9.0 (1): ISC
url@2.5.8 (2): MIT, Apache-2.0
urlencoding@2.1.3 (1): MIT
utf8-ranges@1.0.5 (2): Unlicense, MIT
utf8_iter@1.0.4 (2): Apache-2.0, MIT
utf8parse@0.2.2 (2): Apache-2.0, MIT
uuid@1.26.1 (2): Apache-2.0, MIT
version_check@0.9.5 (2): MIT, Apache-2.0
walkdir@2.5.0 (2): Unlicense, MIT
want@0.3.1 (1): MIT
wasi@0.11.1+wasi-snapshot-preview1 (3): Apache-2.0 WITH LLVM-exception, Apache-2.0, MIT
wasip2@1.0.4+wasi-0.2.12 (3): Apache-2.0 WITH LLVM-exception, Apache-2.0, MIT
wasm-bindgen@0.2.128 (2): MIT, Apache-2.0
wasm-bindgen-futures@0.4.78 (2): MIT, Apache-2.0
wasm-bindgen-macro@0.2.128 (2): MIT, Apache-2.0
wasm-bindgen-macro-support@0.2.128 (2): MIT, Apache-2.0
wasm-bindgen-shared@0.2.128 (2): MIT, Apache-2.0
wasm-streams@0.4.2 (2): MIT, Apache-2.0
web-sys@0.3.105 (2): MIT, Apache-2.0
web-time@1.1.0 (2): MIT, Apache-2.0
webpki-root-certs@1.0.9 (1): CDLA-Permissive-2.0
winapi-util@0.1.11 (2): Unlicense, MIT
windows-core@0.62.2 (2): MIT, Apache-2.0
windows-implement@0.60.2 (2): MIT, Apache-2.0
windows-interface@0.59.3 (2): MIT, Apache-2.0
windows-link@0.2.1 (2): MIT, Apache-2.0
windows-registry@0.6.1 (2): MIT, Apache-2.0
windows-result@0.4.1 (2): MIT, Apache-2.0
windows-strings@0.5.1 (2): MIT, Apache-2.0
windows-sys@0.52.0 (2): MIT, Apache-2.0
windows-sys@0.60.2 (2): MIT, Apache-2.0
windows-sys@0.61.2 (2): MIT, Apache-2.0
windows-targets@0.52.6 (2): MIT, Apache-2.0
windows-targets@0.53.5 (2): MIT, Apache-2.0
windows_aarch64_gnullvm@0.52.6 (2): MIT, Apache-2.0
windows_aarch64_gnullvm@0.53.1 (2): MIT, Apache-2.0
windows_aarch64_msvc@0.52.6 (2): MIT, Apache-2.0
windows_aarch64_msvc@0.53.1 (2): MIT, Apache-2.0
windows_i686_gnu@0.52.6 (2): MIT, Apache-2.0
windows_i686_gnu@0.53.1 (2): MIT, Apache-2.0
windows_i686_gnullvm@0.52.6 (2): MIT, Apache-2.0
windows_i686_gnullvm@0.53.1 (2): MIT, Apache-2.0
windows_i686_msvc@0.52.6 (2): MIT, Apache-2.0
windows_i686_msvc@0.53.1 (2): MIT, Apache-2.0
windows_x86_64_gnu@0.52.6 (2): MIT, Apache-2.0
windows_x86_64_gnu@0.53.1 (2): MIT, Apache-2.0
windows_x86_64_gnullvm@0.52.6 (2): MIT, Apache-2.0
windows_x86_64_gnullvm@0.53.1 (2): MIT, Apache-2.0
windows_x86_64_msvc@0.52.6 (2): MIT, Apache-2.0
windows_x86_64_msvc@0.53.1 (2): MIT, Apache-2.0
winnow@0.7.15 (1): MIT
winnow@1.0.4 (1): MIT
wit-bindgen@0.57.1 (3): Apache-2.0 WITH LLVM-exception, Apache-2.0, MIT
writeable@0.6.4 (1): Unicode-3.0
wyz@0.5.1 (1): MIT
xxhash-rust@0.8.18 (1): BSL-1.0
yoke@0.8.3 (1): Unicode-3.0
yoke-derive@0.8.2 (1): Unicode-3.0
zerocopy@0.8.57 (3): BSD-2-Clause, Apache-2.0, MIT
zerocopy-derive@0.8.57 (3): BSD-2-Clause, Apache-2.0, MIT
zerofrom@0.1.8 (1): Unicode-3.0
zerofrom-derive@0.1.7 (1): Unicode-3.0
zeroize@1.9.0 (2): Apache-2.0, MIT
zerotrie@0.2.5 (1): Unicode-3.0
zerovec@0.11.8 (1): Unicode-3.0
zerovec-derive@0.11.6 (1): Unicode-3.0
zmij@1.0.23 (1): MIT
zstd@0.13.3 (1): MIT
zstd-safe@7.3.0 (1): BSD-3-Clause
zstd-sys@2.1.0+zstd.1.5.7 (1): BSD-3-Clause
```
