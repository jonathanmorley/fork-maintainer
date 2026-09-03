# Build stage
FROM rust:1.95-slim as builder

RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    curl \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY . .

RUN cargo build --release

# Runtime stage
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y \
    ca-certificates \
    git \
    curl \
    && rm -rf /var/lib/apt/lists/*

RUN useradd -m -s /bin/bash fork-maintainer

USER fork-maintainer
WORKDIR /home/fork-maintainer

COPY --from=builder /app/target/release/fork-maintainer /usr/local/bin/

EXPOSE 3000

CMD ["fork-maintainer"]
