FROM rust:1.85-bookworm AS portable
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
RUN cargo build --release --locked && cc -O2 -Wall -Wextra -Werror src/launch.c -o launcher

FROM docker.io/library/gcc:15-bookworm@sha256:9ca91b05c7b07d2979f16413e8b2cd6ec8a7c80ffca4121ccab0aeba33f90460 AS native
RUN apt-get update && apt-get install -y --no-install-recommends python3 liblz4-dev && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY native/*.cpp native/*.hpp native/build.sh native/training.py ./
RUN sh build.sh /usr/local/bin/shopping-sessions && strip /usr/local/bin/shopping-sessions

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends libjemalloc2 liblz4-1 && rm -rf /var/lib/apt/lists/*
COPY --from=portable /build/launcher /usr/local/bin/shopping-sessions
COPY --from=portable /build/target/release/shopping-sessions /usr/local/libexec/shopping-generic
COPY --from=native /usr/local/bin/shopping-sessions /usr/local/libexec/shopping-native
COPY native/LICENSE /usr/share/doc/shopping-sessions/native-LICENSE
ENV MALLOC_CONF=thp:always,narenas:1
ENV DATA_DIR=/data OUTPUT_DIR=/output SCRATCH_DIR=/scratch HOME=/scratch TMPDIR=/scratch NCPU=8
USER 1000:1000
ENTRYPOINT ["/usr/local/bin/shopping-sessions"]
