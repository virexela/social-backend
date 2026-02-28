# syntax=docker/dockerfile:1

FROM --platform=linux/amd64 rust:1.88-slim-bookworm AS builder
WORKDIR /app

# Faster builds with dependency caching
COPY Cargo.toml Cargo.lock* ./
RUN mkdir -p src && echo "fn main() {}" > src/main.rs
RUN cargo build --release

# Build actual binary
COPY src ./src
RUN cargo build --release

FROM --platform=linux/amd64 debian:bookworm-slim
RUN apt-get update \
  && apt-get install -y --no-install-recommends ca-certificates \
  && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder /app/target/release/social-backend /app/social-backend

ENV BIND_ADDR=0.0.0.0:3000
EXPOSE 3000

CMD ["/app/social-backend"]
