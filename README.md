# Shopping Sessions

A Rust implementation of the Shopping Sessions challenge. It reads the mounted
CSV inputs, resolves linked IDs, and writes both required Parquet tables.
The exact behavior is specified in [rules.md](rules.md). Current verification
results and remaining work are recorded in [PROGRESS.txt](PROGRESS.txt).

The image starts a compiled executable with no arguments. Dependencies are
installed during the build. Runtime diagnostics go to stderr. Input dates,
identities, products, campaigns, and file counts all come from the mounted data.

## Build and run

Use Linux or WSL with Docker. The final image targets linux/amd64.

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

Parser unit checks are available with `make test` when Rust 1.85 or later is
installed. `Cargo.lock` pins the dependency graph used by the Docker build.

## Local measurements

The final image was tested through WSL/Docker on a laptop, with 8 pinned CPU
threads, a 6 GiB memory limit, no swap, read-only root and inputs, uid 1000,
and disabled networking. Wall time below includes the Docker CLI lifecycle.

| Generated dataset | Wall time | Pipeline time | Peak container memory |
| --- | --- | --- | --- |
| Seed 2026, 20M events and 5M clicks | 13.93 s | 12.484 s | 3.62 GiB |
| Seed 99, 60M events and 15M clicks | 54.82 s | 52.293 s | 6.00 GiB |

These are local measurements. Hidden scorer timings remain unmeasured.
Both large outputs pass schema, unique-key, and cross-table total checks.
Exact answer comparisons use the 36 fixtures, the official million-event
answer, and the independent reference on three additional small datasets
and an adversarial case. The large datasets have no published golden answers.

For the 60M input, a scratch copy of the official generator wrote shards
incrementally to fit local RAM. Its seed-1 output matched all 87 original
generator files byte for byte before use. Generation is outside timed runs.

## How the implementation works

1. `src/parse.rs` implements the specified CSV dialect and timestamp formats.
   Empty fields, quotes, ASCII digits, leap years, offsets, and CR bytes are
   handled explicitly. Timestamps are integer UTC milliseconds.
2. `src/input.rs` reads every CSV part and resolves identity links with
   union-find. Component names are the byte-wise smallest linked IDs. The
   pipeline interns all observed user IDs as integers, including singletons,
   so later sorting can work from compact records. Read-only mappings keep
   borrowed event and click IDs valid during processing.
3. `src/main.rs` partitions events by event ID and retains the earliest
   ingestion globally. Invalid records remain eligible to win deduplication.
   Validation and lateness determine whether the winning record survives.
   Short event IDs are stored directly in each record. Longer IDs retain
   their full borrowed values. Both representations use full byte equality.
4. Surviving events and valid clicks are partitioned by canonical person.
   Sorting by person and time makes session formation an adjacent-gap scan.
   A session's first timestamp determines the day of all its purchases.
5. For each person's increasing session starts, two click indices maintain
   the inclusive seven-day window. A count per campaign tracks active assists.
   The latest timestamp and smallest click ID determine the credited campaign.
   Each click enters and leaves the window at most once. Assist accumulation
   visits each active campaign once per session.
6. `src/output.rs` writes compressed Parquet with explicit DATE columns and
   nullable top_category. Campaign totals combine credited and assisted rows.

Global event deduplication precedes partitioning by person because later copies
can name a different user. Equal event timestamps stay together, and purchases
after midnight remain assigned to their session's start day. String comparisons
preserve the original UTF-8 bytes throughout.

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
