# The dedicated arena server sample for the Cloudflare Containers adapter.
#
# Build context: this Dockerfile lives in examples/arena-dedicated/server/ but
# must be built from the REPOSITORY ROOT, because the sample path-depends on the
# hearth-* crates and their manifests inherit the root workspace:
#
#   docker build -f examples/arena-dedicated/server/Dockerfile .
#
# The sibling Dockerfile.dockerignore keeps the context small. The `[[containers]]`
# entry in the lobby's wrangler.toml points at
# `image = "./server/Dockerfile"` with that same repo-root context.

FROM rust:1.91-slim AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY examples ./examples
WORKDIR /src/examples/arena-dedicated/server
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/examples/arena-dedicated/server/target/release/arena-dedicated-server /usr/local/bin/hearth-arena-server
ENV HEARTH_PORT=8080
EXPOSE 8080
CMD ["/usr/local/bin/hearth-arena-server"]
