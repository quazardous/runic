# syntax=docker/dockerfile:1.7

FROM rust:alpine AS builder
WORKDIR /build
RUN apk add --no-cache musl-dev

# Dep cache layer: build with a stub main.rs so the deps tree gets cached as long as
# the manifests don't change. The real build below only recompiles our crate.
#
# The root Cargo.toml is a workspace whose only other member is the Windows-only
# `runic-tray`. We never build the tray in this Linux image (always `-p runic`),
# but cargo still has to PARSE every workspace member's manifest, so we copy
# runic-tray/Cargo.toml and give it a stub bin. Cargo.lock is copied so the build
# is `--locked` (reproducible, and matches the published release).
COPY Cargo.toml Cargo.lock ./
COPY runic-tray/Cargo.toml runic-tray/Cargo.toml
RUN mkdir -p src runic-tray/src \
 && echo 'fn main() { println!("stub"); }' > src/main.rs \
 && echo '' > src/lib.rs \
 && echo 'fn main() {}' > runic-tray/src/main.rs \
 && cargo build --release --locked -p runic \
 && rm -rf src target/release/runic target/release/deps/runic*

COPY src ./src
RUN touch src/main.rs src/lib.rs && cargo build --release --locked -p runic

FROM gcr.io/distroless/static-debian12:nonroot
COPY --from=builder /build/target/release/runic /usr/local/bin/runic
EXPOSE 7878
# Admin API + status page (off by default to loopback inside the container; bind
# admin.addr to 0.0.0.0 and publish this port to reach it from the host).
EXPOSE 48484
ENTRYPOINT ["/usr/local/bin/runic"]
CMD ["--config", "/etc/runic/runic.yaml"]
