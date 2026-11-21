# Shopping Sessions

A Rust implementation of the Shopping Sessions challenge. It reads the mounted
CSV inputs, resolves linked IDs, and writes both required Parquet tables.
The exact behavior is specified in [rules.md](rules.md). Current verification
results and remaining work are recorded in [PROGRESS.txt](PROGRESS.txt).

The image starts a compiled executable with no arguments. Dependencies are
installed during the build. Runtime diagnostics go to stderr. Input dates,
identities, products, campaigns, and file counts all come from the mounted data.

## Build and run

Use Linux or WSL with Docker. The linux/amd64 image selects its CPU target at
startup. It includes a build for x86-64-v3 with AES and a generic amd64 build.

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

CSV, timestamp, identity and compact-record unit checks are available with `make test` when Rust 1.85 or later is
installed. `Cargo.lock` pins the dependency graph used by the Docker build.

## Local measurements

The team reported an 8-second practice score for the earlier commit
`d6783e0`. These measurements compare that image with this one on a Ryzen 5
5600H laptop. Each run used a fresh container, eight pinned hardware threads
across four cores, 6 GiB, no swap, read-only inputs and disabled networking.

| Official generator, seed 2026, 20M events + 5M clicks | Previous | Optimized |
| --- | ---: | ---: |
| Warm Linux filesystem, median of three per image | 9.538 s | 2.009 s |
| Linux input page cache evicted, one run per image | 9.850 s | 4.009 s |

Times use Docker's container start and exit timestamps. The warm trials ran
in alternating order on identical input. The optimized times were 1.954,
2.009 and 2.084 seconds. Before each cache-eviction trial, Linux reported
zero of 459,975 input pages resident. The host's underlying storage cache
was uncontrolled.

Scaling the team's 8-second score by each measured ratio gives 1.69 seconds
with warm input and 3.26 seconds after eviction. A practical estimate is
2�3 seconds, with a wider 1.5�4 second planning range. The hidden seed,
hardware, and input cache state determine the official score.

A separate 60M-event, 15M-click run on seed 99 used a Windows-mounted
directory for both images. Docker CLI wall time fell from 54.82 to
28.63 seconds and peak container memory fell from 6.00 to 4.52 GiB.
On a native Linux filesystem, three optimized containers took 33.552,
12.051 and 11.241 seconds from start to exit (median 12.051 seconds).
The WSL VM has 7.4 GiB total. File cache and the process compete for RAM
at final scale.

Both tables matched the public million-event answer and all 36 fixtures.
Three extra seeds, a long identity chain, compact and full-width duplicate
records, and sparse campaign totals matched the independent Python reference.
Every output key and value at 20M and 60M matched the earlier verified image,
including 2,224,281 and 6,648,247 user/day rows respectively and 28,014
campaign/day rows per dataset. Large generated datasets have no published
golden answers.

For the 60M input, a scratch copy of the official generator wrote shards
incrementally to fit local RAM. Its seed-1 output matched all 87 original
generator files byte for byte before use. Generation is outside timed runs.

## How the implementation works

1. Read every CSV shard and skip its own header. The tuned parser scans comma,
   newline and quote positions with AVX2. It checks that quoted fields open
   and close at the correct boundaries. Longer rows use the same strict scalar
   rules. Calendar validation is cached by date and timestamps retain millisecond
   precision.
2. Build the transitive graph of linked user IDs with union-find while other
   threads scan the events. Keep the smallest UTF-8 name in each component.
   Short user names fit in a machine word. All other names retain their exact
   bytes in an interner. An event can be scanned before the graph finishes
   because its raw user key can be resolved afterward.
3. Partition by **event ID** and choose the earliest ingestion across all files.
   An invalid earliest copy still suppresses a later valid copy, even if the
   copies name different users. Two bitmaps identify hash slots that occur only
   once. A repeated slot always goes through exact key comparisons. The common
   nonpurchase record occupies 24 bytes including its event key. Purchases,
   longer IDs and wider time ranges have full-width representations.
4. Partition surviving events and valid clicks by canonical person. Sort each
   partition by person and time, breaking click ties by original ID bytes.
   An adjacent gap greater than 1,800,000 ms starts a session. All its events
   and revenue belong to the UTC day on which it started.
5. Move two indices through the person's sorted clicks as session starts
   advance. The inclusive seven-day window supplies both the last eligible
   click and all the distinct assisting campaigns. Small campaign/day tables
   use direct indexing. Large calendars or catalogs use a sparse table.
6. Write each shard's user rows directly to Parquet. Consecutive rows for a
   person share one dictionary entry and a run-length encoded index. Merge
   campaign counts across shards and write the campaign table. Empty outputs
   still carry the required schema.

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
