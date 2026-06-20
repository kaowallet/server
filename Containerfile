# Build
FROM docker.io/library/rust:1-bookworm AS build
WORKDIR /src

# Cache dependencies separately from source.
COPY Cargo.toml Cargo.lock* ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release \
    && rm -rf src

COPY src ./src
# Touch so cargo rebuilds with the real source.
RUN touch src/main.rs && cargo build --release

# Runtime: distroless/cc (glibc, no shell, no package manager).
# Mozilla roots are compiled into the binary via reqwest's webpki-roots feature,
# so no ca-certificates package is required.
FROM gcr.io/distroless/cc-debian12:nonroot AS runtime
COPY --from=build /src/target/release/kao-proxy /usr/local/bin/kao-proxy
USER nonroot:nonroot
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/kao-proxy"]
