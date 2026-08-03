# Build stage - use Alpine + musl for fully static binary
FROM rust:latest AS builder

RUN apt-get update && apt-get install -y musl-tools && rm -rf /var/lib/apt/lists/*
RUN rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl

WORKDIR /app

# Detect architecture and set target
ARG TARGETARCH
RUN case "$TARGETARCH" in \
      arm64) echo "aarch64-unknown-linux-musl" > /tmp/rust-target ;; \
      *)     echo "x86_64-unknown-linux-musl" > /tmp/rust-target ;; \
    esac

# Cache dependencies by copying manifests first
COPY Cargo.toml Cargo.lock* ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release --target $(cat /tmp/rust-target) \
    && rm -rf src

# Build the actual application
COPY src/ src/
RUN touch src/main.rs \
    && cargo build --release --target $(cat /tmp/rust-target) \
    && cp target/$(cat /tmp/rust-target)/release/redis-tui /redis-tui

# Runtime stage - scratch since the binary is fully static
FROM alpine:latest AS runtime

COPY --from=builder /redis-tui /usr/local/bin/redis-tui

CMD ["redis-tui"]

############################
# Publish stage: single-owner squash
############################
# Rootless podman without subordinate UID/GID ranges cannot chown
# extracted layer files to unmapped IDs, so layers containing any
# non-root-owned files fail to pull without storage.conf changes.
# Squashing the finished rootfs through scratch with --chown=0:0 makes
# every file root-owned in one layer: extraction only chowns to the
# caller's own mapped UID, so the image runs on stock rootless podman
# with zero host configuration. Nothing here needs setuid or per-user
# ownership, so the squash is lossless.
FROM scratch AS publish
COPY --from=runtime --chown=0:0 / /

CMD ["redis-tui"]
