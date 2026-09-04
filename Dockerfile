# Build stage
FROM rust:1.98-slim@sha256:17d1ba895198f9934c6314ec5346a0d5115372f3243390c3d731e242f35c2f27 AS builder

RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    curl \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY . .

RUN cargo build --release

# Runtime stage.
#
# IMPORTANT: the runtime base must ship a glibc at least as new as the one the
# builder links against. `rust:1.95-slim` builds against glibc 2.39+ (Debian
# 13/trixie era). debian:bookworm-slim ships glibc 2.36 and will fail at
# runtime with "GLIBC_2.39 not found". Using trixie matches the builder.
FROM debian:trixie-slim

RUN apt-get update && apt-get install -y \
    ca-certificates \
    git \
    curl \
    && rm -rf /var/lib/apt/lists/*

RUN useradd -m -s /bin/bash fork-maintainer

USER fork-maintainer
WORKDIR /home/fork-maintainer

COPY --from=builder /app/target/release/fork-maintainer /usr/local/bin/

# Render expects web services to listen on port 10000 by default. The app's
# default (3000) is overridden in the Render blueprint; keep them in agreement
# so `docker run -p` matches production.
EXPOSE 10000

CMD ["fork-maintainer"]
