# Multi-stage build for StormBlock container image.
# Produces a self-contained Alpine image with storage tools + stormblock binary.
# Managed by StormBase for deployment and updates.

# --- Build stage ---
FROM rust:1.75-alpine AS builder

RUN apk add --no-cache musl-dev

WORKDIR /build
COPY . .

RUN cargo build --release --target x86_64-unknown-linux-musl && \
    strip target/x86_64-unknown-linux-musl/release/stormblock

# --- Runtime stage ---
FROM alpine:3.21

RUN apk add --no-cache \
    nvme-cli smartmontools fio iproute2 util-linux \
    lsblk e2fsprogs xfsprogs jq ca-certificates

COPY --from=builder /build/target/x86_64-unknown-linux-musl/release/stormblock /usr/bin/stormblock
COPY --from=builder /build/stormblock.example.toml /etc/stormblock/stormblock.toml

EXPOSE 3260 4420 9090

ENTRYPOINT ["/usr/bin/stormblock"]
