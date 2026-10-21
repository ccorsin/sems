# Multi-stage build. Default target: the minimal runtime image (last stage).
#   docker build -t sems .                 → runtime image
#   docker build --target tester .         → test image (used by `docker compose run --rm test`)

# Dependencies are compiled against a dummy crate first, so that code changes
# do not rebuild the ~155 dependency crates (Docker layer cache).
FROM rust:1.96-slim-bookworm AS base
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && touch src/lib.rs

FROM base AS builder
RUN cargo build --release
COPY . .
# `touch`: make cargo see the real sources as newer than the dummy build.
RUN touch src/main.rs src/lib.rs && cargo build --release

# Debug profile on purpose: the `debug_assert!` safety invariant stays active in tests.
FROM base AS tester
RUN cargo test --no-run
COPY . .
RUN touch src/main.rs src/lib.rs && cargo test --no-run
CMD ["cargo", "test"]

# Runtime: binary + default config only. No shell, non-root user.
FROM gcr.io/distroless/cc-debian12:nonroot AS runtime
COPY --from=builder /app/target/release/sems /sems
COPY config/station.json /config/station.json
EXPOSE 8080
ENTRYPOINT ["/sems"]
CMD ["/config/station.json"]
