FROM rust:1.85-bookworm AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
# Select for the runner's CPU. The build host can be a different model.
RUN cc -O2 -Wall -Wextra -Werror src/launch.c -o /build/launcher \
    && RUSTFLAGS='-C target-cpu=x86-64-v3 -C target-feature=+aes' cargo build --release --locked \
    && cp target/release/shopping-sessions /build/tuned \
    && cargo build --release --locked --offline

# Build host tools for their native CPU. The competition binary alone uses v4.
FROM rust:1.100-bookworm AS build-v4
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
RUN RUSTFLAGS='-C target-cpu=x86-64-v4 -C target-feature=+aes' \
    cargo build --release --locked --target x86_64-unknown-linux-gnu

FROM debian:bookworm-slim
RUN useradd --uid 1000 --no-create-home --home-dir /scratch app
COPY --from=build /build/launcher /usr/local/bin/shopping-sessions
COPY --from=build-v4 /build/target/x86_64-unknown-linux-gnu/release/shopping-sessions /usr/local/libexec/shopping-v4
COPY --from=build /build/tuned /usr/local/libexec/shopping-tuned
COPY --from=build /build/target/release/shopping-sessions /usr/local/libexec/shopping-generic
# Ask glibc to use transparent huge pages for eligible heap allocations.
# The kernel chooses the backing page size, including on hosts without THP.
ENV GLIBC_TUNABLES=glibc.malloc.hugetlb=1
ENV DATA_DIR=/data OUTPUT_DIR=/output SCRATCH_DIR=/scratch HOME=/scratch TMPDIR=/scratch
USER 1000:1000
CMD ["/usr/local/bin/shopping-sessions"]
