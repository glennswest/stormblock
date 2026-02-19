# Multi-stage build for StormBlock container image.
# Managed by StormBase for deployment and updates.

# --- Build stage ---
FROM rust:1.75-alpine AS builder

RUN apk add --no-cache musl-dev

WORKDIR /build
COPY . .

RUN cargo build --release --target x86_64-unknown-linux-musl && \
    strip target/x86_64-unknown-linux-musl/release/stormblock

# --- Runtime stage ---
FROM scratch

COPY --from=builder /build/target/x86_64-unknown-linux-musl/release/stormblock /stormblock
COPY --from=builder /build/stormblock.example.toml /etc/stormblock/stormblock.toml

EXPOSE 3260 4420 9090

ENTRYPOINT ["/stormblock"]
