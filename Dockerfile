## ── Stage 1: Build ──
FROM rust:1.75-bookworm AS builder

WORKDIR /app

# Cache dependencies — copy manifests first
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && \
    cargo build --release 2>/dev/null || true && \
    rm -rf src

# Copy real source and build
COPY src/ ./src/
COPY static/ ./static/
# Touch main.rs to force rebuild with actual code
RUN touch src/main.rs && cargo build --release

## ── Stage 2: Runtime ──
FROM debian:bookworm-slim

RUN apt-get update && \
    apt-get install -y --no-install-recommends \
        ffmpeg \
        curl \
        ca-certificates && \
    rm -rf /var/lib/apt/lists/*

# Create non-root user
RUN useradd -m -s /bin/bash clipper

COPY --from=builder /app/target/release/video-clipper /usr/local/bin/video-clipper

# Data dir. Deliberately no `VOLUME /data`: Railway rejects the instruction
# ("use Railway Volumes"), and it isn't needed — docker-compose mounts
# clipper-data:/data explicitly, Railway mounts its volume at /data.
RUN mkdir -p /data && chown clipper:clipper /data

USER clipper
WORKDIR /data

ENV DATA_DIR=/data \
    HOST=0.0.0.0 \
    PORT=8080 \
    STT_MODEL=google/gemini-2.5-flash \
    ANALYSIS_MODEL=anthropic/claude-sonnet-4 \
    MAX_CONCURRENT_JOBS=2 \
    MAX_UPLOAD_MB=4096 \
    RUST_LOG=info

EXPOSE 8080

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD curl -sf http://localhost:8080/health || exit 1

ENTRYPOINT ["video-clipper"]
