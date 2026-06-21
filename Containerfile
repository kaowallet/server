# syntax=docker/dockerfile:1

# Build
FROM docker.io/library/rust:1-bookworm AS build
WORKDIR /src

# Only the manifests and source enter the build context (see .dockerignore).
COPY Cargo.toml Cargo.lock ./
COPY src ./src

# Cache mounts persist the crate registry and the target dir across builds, so a
# source-only change recompiles just the local crate instead of every dependency.
# target/ is a mount (not committed to the layer), so copy the binary out of it
# within the same RUN. --locked fails the build if Cargo.lock is stale.
# Requires BuildKit (Docker 23+ default) or Buildah/Podman.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target,sharing=locked \
    cargo build --locked --release \
    && cp target/release/kao-proxy /usr/local/bin/kao-proxy

# Runtime: distroless/cc (glibc, no shell, no package manager).
# Mozilla roots are compiled into the binary via reqwest's webpki-roots feature,
# so no ca-certificates package is required.
FROM gcr.io/distroless/cc-debian12:nonroot AS runtime
COPY --from=build /usr/local/bin/kao-proxy /usr/local/bin/kao-proxy
USER nonroot:nonroot
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/kao-proxy"]
