# Shopping Sessions

This branch runs a C++20 Shopping Sessions pipeline on x86-64-v3 hardware
and the team's Rust implementation on older amd64 machines. It reads the
mounted CSV inputs, resolves linked IDs, and writes both required Parquet tables.
The exact behavior is specified in [rules.md](rules.md). Current verification
results and remaining work are recorded in [PROGRESS.txt](PROGRESS.txt).

The native pipeline is derived from [Shopping Sessions contributors' public
C++ solver](https://github.com/mliao0128/fraun-shopping-sessions/tree/dd510eadd036edcc10af568ad6dd3d656ae98f73)
at revision `dd510eadd036edcc10af568ad6dd3d656ae98f73`. Its copyright
and MIT permission notice are retained in [native/LICENSE](native/LICENSE)
and in the runtime image. The C++ build uses GCC, jemalloc and LZ4.

The native linker stores each redirected shopper ID once. It resolves
conflicting links to the same bytewise-minimum name before routing events.
The event deduplicator indexes rows by compact handles and compares the full
event ID before choosing the earliest arrival. It records the arrival of
discarded events while the input is still in cache, so those copies can
still win deduplication. Large partitions and long probe chains use the
general deduplicator. Compiler training uses synthetic inputs created during
the build.

Each compact arrival stores the UTC day and six clock digits in chronological
order. Most duplicate comparisons can use that order directly. The parser
converts the clock to milliseconds when a row crosses a lateness boundary
or when comparing to a full-width arrival. For event timestamps with UTC
offsets, SIMD instructions check the local clock and offset together. The
Parquet writer packs day and count values directly into its encoded pages.

The image starts a compiled executable with no arguments. Dependencies are
installed during the build. Runtime diagnostics go to stderr. Input dates,
identities, products, campaigns, and file counts all come from the mounted data.

## Build and run

Use Linux or WSL with Docker. The launcher selects the executable from the
running CPU's capabilities. The C++ build uses profile-guided optimization
on build hosts that support x86-64-v3.

```sh
make build
make run DATA=/path/to/input OUT=/path/to/output
```

The runner applies the scorer's network, filesystem, user, process, and memory
restrictions. Its defaults are 8 CPUs and 24 GiB. A laptop can use lower limits.

```sh
make run DATA=/path/to/input OUT=/path/to/output CPUS=8 MEM=6g
```

Use an empty output directory for each dataset, as the scorer does. The
container writes directly to the two table folders under that directory.

The executable reads `DATA_DIR` and `OUTPUT_DIR`, defaulting to `/data` and
`/output`. `NCPU` controls its worker count. Temporary storage can be mounted
at `/scratch`. The current pipeline keeps its intermediate records in memory.

## Verify

The checkers need Python 3 and the packages in `requirements-dev.txt`. The
36 supplied fixtures include their original expected outputs.

```sh
python3 -m pip install -r requirements-dev.txt
make fixtures
```

Build the official generator from the provided generator.zip and place
`generate` on PATH. Extract dev-answer.zip to a separate directory, then run
the public comparison.

```sh
make dev DATA=/path/to/dev
make run DATA=/path/to/dev OUT=/path/to/dev-output
make check-dev OUT=/path/to/dev-output ANSWER=/path/to/dev-answer
make check-totals OUT=/path/to/dev-output
```

`scripts/reference.py` is an independent Python implementation for small
generated datasets. It uses graph traversal and exhaustive click matching,
which makes it useful for checking the optimized implementation on new seeds.

```sh
generate --seed 42 --events 30000 --clicks 10000 --output /path/to/seed42
python3 scripts/reference.py /path/to/seed42 /path/to/expected42
make run DATA=/path/to/seed42 OUT=/path/to/output42
python3 scripts/compare.py /path/to/output42 /path/to/expected42
```

Run `make test-native` with a C++20 compiler on an x86-64-v3 Linux machine
for the native unit checks. `make test` runs the Rust checks with Rust 1.85
or later. `Cargo.lock` pins the Rust dependency graph used by the Docker build.

## Rust implementation and previous submissions

The following implementation notes and timings describe the Rust solver.

### Event parsing and identity links

The event reader validates quotes and field boundaries while extracting the
fields needed for common view and cart rows. It decodes event and arrival
clocks together when they share a UTC date. Other layouts use the complete
field parser. Invalid events still participate in earliest-arrival selection.

The identity table initially points each larger ID at one smaller neighbor.
When two links give it different parents, the solver joins their components.
It then stores each ID's final canonical name for event and click lookups.

## Worker memory reuse

The team scored **1.05 s** with commit `b86512b`.
Each worker reuses its event, click and campaign buffers across person shards.
A compact index lets users with no possible identity redirect skip a hash-table
lookup. The packaged build matched both complete output tables at 20M and 60M.
In a five-run practice comparison, the three local medians stayed within 2.3%
of the preceding release. For 60M events and 15M clicks, the local median was
6.243 s over three runs with 6 GiB. See [PROGRESS.txt](PROGRESS.txt) for the
individual final-size times and test coverage.

## Benchmarks for the 1.08 s submission

The team scored **1.08 s on the portal** with `9a8f5a0` and **1.17 s** with
`b45f23d`. Each number below
is a median of fresh, restricted containers on the Ryzen 5 5600H, pinned to
eight hardware threads across four cores with 6 GiB and no swap. We alternated
images on identical files and measured Docker start to exit. We read the
files before each warm series. Seed 271828 was held out of implementation
and configuration selection.

| Practice input (20M events + 5M clicks) | Runs per image | `b45f23d` | `9a8f5a0` | Reduction |
|---|---:|---:|---:|---:|
| Warm, seed 2026 | 5 | 1.599 s | **1.471 s** | 8.1% |
| Warm, seed 73 | 5 | 1.587 s | **1.514 s** | 4.6% |
| Warm, new seed 271828 | 5 | 1.686 s | **1.440 s** | 14.6% |
| Linux input cache evicted, seed 2026 | 3 | 3.389 s | 3.482 s | -2.7% |

The parser validates all quotes with bit masks, reads each field's opening
byte for trimming, and decodes hours, minutes and seconds in parallel SIMD
lanes on supported CPUs. Ingestion timestamps stay in whole seconds for
duplicate comparisons. Event IDs and payloads use separate contiguous
buffers per source chunk, with an overflow vector for any partition that
exceeds its estimated capacity. The next two stages schedule the largest
groups first to reduce the time workers spend waiting for the last group.

The first paired final-size trial (60M events, 15M clicks, seed 99) had a
median of **5.333 s** for this build and **4.816 s** for `b45f23d`. Each
image ran three times with 6 GiB. A second paired batch measured 5.768 s
for this release and 5.686 s for the old one. A 60M speedup on this laptop
remains unverified. The competition provides 24 GB. Both output
tables matched on all three practice inputs and the final-size input,
including all 6,648,247 user/day rows at final size. The container passed
all 36 public fixtures, and both CPU builds matched the public 1M answer.

The earlier AVX-512 comparison branch first verified CPU dispatch and the
64-byte CSV classifier under Intel SDE. The current image includes this path.

## Local measurements for the 1.17 s submission

The team reports **1.52 s on the portal** for commit `36e2f49` and 8 s
for its predecessor `d6783e0`. The comparisons below use `36e2f49` and
`b45f23d` on a Ryzen 5 5600H. Each sample is a fresh container with eight
pinned hardware threads on four cores, 6 GiB, no swap, read-only input and
no network. Durations come from Docker's start and exit timestamps.

| 20M events + 5M clicks | Runs per image | 1.52 s submission | 1.17 s submission |
|---|---:|---:|---:|
| Seed 2026, warm Linux input | 5 | 1.985 s | **1.275 s** |
| Seed 73, warm Linux input | 3 | 1.972 s | **1.267 s** |
| Seed 2026, Linux input cache evicted | 3 | 4.487 s | 4.026 s |

All values are medians. Old and new containers ran in interleaved order on
identical input. Seed 73 was held out of tuning. Linux reported zero of the
459,975 input pages resident before every evicted run. The underlying
storage cache was uncontrolled. The fastest practice-scale release trial
took 1.189 s from start to exit and 0.996 s inside the executable.

Applying the warm runtime ratios to the team's 1.52 s portal score gives
**about 0.98 s** on either seed. The cache-evicted ratio gives 1.36 s.
The team's subsequent official result for `b45f23d` was **1.17 s**.

On final-size seed 99 (60M events and 15M clicks), three optimized
containers took 12.070, 9.587, 5.450 s. The median was **9.587 s**.
The WSL VM has 7.4 GiB RAM. Input pages compete with working memory at
this size, which contributes to the spread between runs.

The release passed all 36 official fixtures and both CPU builds matched
the public million-event answer exactly. Twelve additional datasets covered
three independent seeds and nine adversarial cases, including fingerprint
collisions and timestamp-width boundaries.
Both entire output tables matched the previous verified implementation on
the two 20M seeds and the 60M seed. Large generated inputs have no published
golden answers.

For the 60M input, a scratch copy of the official generator wrote shards
incrementally to fit local RAM. Its seed-1 output matched all 87 original
generator files byte for byte before use. Generation is outside timed runs.

## How the implementation works

1. Read each CSV shard and skip its own header. The tuned parser classifies
   commas, newlines and quotes in blocks, and checks both quote boundaries.
   Validated calendar dates are cached. The timestamp path also handles integer
   epoch milliseconds and UTC offsets. Long rows use a scalar parser.
2. Parse identity edges while reading event rows. An open-addressed table
   doubles as a union-find forest. Roots are selected by UTF-8 byte order.
   After union, each occupied slot contains its canonical name, including
   identities that only appear in the link files.
3. Partition event IDs independently of their payloads. An ID entry records
   the ingestion time and the location of its payload. A bitmap slot seen
   once proves a row is unique. For repeated slots, an owner table compares
   the full ID before suppressing a later copy. An invalid earliest copy
   still suppresses later valid ones. Payloads are grouped by raw user key
   so nearby identity lookups share cache lines.
4. Group surviving events and valid clicks by canonical person. In short
   windows, an event's millisecond offset, price and category fit in a single
   64-bit value after grouping. A grouped click uses 16 bytes and retains
   a locator for its original ID. Wider timestamps and catalogs use larger
   representations. Only a timestamp tie requires reading click ID bytes.
5. Sort ordinary shoppers' events by time. For high-volume shoppers in a
   bounded window, aggregate into half-hour bins. Every event within one bin
   belongs to one session. Two occupied bins join when the next minimum
   timestamp is within 30 minutes of the previous maximum. The session's
   first timestamp sets its UTC reporting date.
6. Advance two indices over each person's clicks. The inclusive seven-day
   window determines the latest eligible click and the distinct assisting
   campaigns. Each shard writes its user/day rows and returns campaign totals
   for reduction. The writer uses dictionary and run-length encoding for user
   IDs, dictionary encoding for small category catalogs, and delta encoding
   for integers. Empty tables still carry the required schema.

For example, a phone view at 23:55 and a linked laptop purchase at 00:10
belong to one session on the first date. If campaign A had a click two days
earlier and B had the latest click at 23:54, B receives the session and
purchase revenue. A and B each receive one assist. A click at 23:56 is too
late to receive credit for this session, even though it precedes the purchase.

For background on classifying many input bytes into structural masks at once,
see Langdale and Lemire's [SIMD parsing paper](https://arxiv.org/abs/1902.08318).
The user-ID writer follows the [Apache Parquet dictionary and RLE formats](https://parquet.apache.org/docs/file-format/data-pages/encodings/).

## Storage and submission

On the development machine, generated inputs, outputs, tool downloads, and
build caches are under `D:\ChatGPT-Codex\tmp\shopping-sessions`. Project source
and the supplied fixtures live in this workspace. Set `TMPDIR`, or `TEMP` and
`TMP` on Windows, to a task directory on D: when running local checkers.

The repository's root Dockerfile builds the submission image. Commit source,
the lockfile, scripts, and the small supplied fixtures. Generated datasets,
build outputs, and downloaded ZIP archives are excluded by `.gitignore`.
Keep the selected commit available in the public repository through judging.
Public repository publication and a scored submission are separate steps.
