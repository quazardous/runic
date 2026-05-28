# syntax=docker/dockerfile:1.7

FROM rust:alpine AS builder
WORKDIR /build
RUN apk add --no-cache musl-dev

# Dep cache layer: build with a stub main.rs so the deps tree gets cached as long as
# Cargo.toml doesn't change. The real build below only recompiles our crate.
COPY Cargo.toml ./
RUN mkdir src \
 && echo 'fn main() { println!("stub"); }' > src/main.rs \
 && cargo build --release \
 && rm -rf src target/release/runic target/release/deps/runic*

COPY src ./src
RUN touch src/main.rs && cargo build --release

FROM gcr.io/distroless/static-debian12:nonroot
COPY --from=builder /build/target/release/runic /usr/local/bin/runic
EXPOSE 7777
ENTRYPOINT ["/usr/local/bin/runic"]
CMD ["--config", "/etc/runic/runic.yaml"]
