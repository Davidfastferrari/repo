# -------- STAGE 1: BUILD --------
FROM rust:1.76 as builder

WORKDIR /app

# Pre-cache build
COPY Cargo.toml .
COPY Cargo.lock .
RUN mkdir src && echo "fn main() {}" > src/main.rs
RUN cargo build --release || true

# Full source copy
COPY src/ src/
COPY benches/ benches/
COPY contracts/ contracts/
RUN cargo build --release

# -------- STAGE 2: RUN --------
FROM debian:bookworm-slim  # <- OpenSSL 3 available here ✅

RUN apt-get update && apt-get install -y \
    libssl3 \
    ca-certificates \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder /app/target/release/rust /app/

CMD ["./rust"]
