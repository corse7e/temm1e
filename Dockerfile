# ================================================
# SkyClaw Multi-stage Dockerfile
# Cloud-native Rust AI agent runtime
# Target: <50 MB final image, <50 ms cold start
# ================================================

# ----- Stage 1: Chef planner -----
FROM rust:1.83-slim AS chef
RUN cargo install cargo-chef --locked
RUN apt-get update && apt-get install -y musl-tools && rm -rf /var/lib/apt/lists/*
RUN rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
WORKDIR /app

# ----- Stage 2: Dependency planner -----
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# ----- Stage 3: Builder (cached dependencies) -----
FROM chef AS builder

# Determine musl target based on build platform
ARG TARGETARCH
RUN if [ "$TARGETARCH" = "arm64" ]; then \
      echo "aarch64-unknown-linux-musl" > /tmp/rust-target; \
    else \
      echo "x86_64-unknown-linux-musl" > /tmp/rust-target; \
    fi

# Cook dependencies first (cached layer)
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release \
    --target $(cat /tmp/rust-target) \
    --recipe-path recipe.json

# Build the actual binary
COPY . .
RUN cargo build --release \
    --target $(cat /tmp/rust-target) \
    --bin skyclaw && \
    cp target/$(cat /tmp/rust-target)/release/skyclaw /app/skyclaw-bin

# ----- Stage 4: Runtime (minimal) -----
FROM alpine:3.19 AS runtime

# Install curl for health checks, ca-certificates for TLS
RUN apk add --no-cache curl ca-certificates && \
    addgroup -S skyclaw && \
    adduser -S -G skyclaw skyclaw

# Copy binary and config
COPY --from=builder /app/skyclaw-bin /usr/local/bin/skyclaw
COPY config/default.toml /etc/skyclaw/default.toml

# Create data directory
RUN mkdir -p /var/lib/skyclaw && \
    chown -R skyclaw:skyclaw /var/lib/skyclaw /etc/skyclaw

USER skyclaw
WORKDIR /var/lib/skyclaw

# Gateway port
EXPOSE 8080

# Health check: hit the gateway endpoint
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD curl -f http://localhost:8080/health || exit 1

ENTRYPOINT ["skyclaw"]
CMD ["start"]
