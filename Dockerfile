# -------- STAGE 1: BUILD --------
FROM rust:1.76 as builder

WORKDIR /app

# Copy manifest files and dummy src to warm up the dependency cache
COPY Cargo.toml .
COPY Cargo.lock .

# Create a dummy src/main.rs for cache
RUN mkdir src && echo "fn main() {}" > src/main.rs
RUN cargo build --release || true

# Now copy actual project files
COPY benches/ benches/
COPY src/ src/
COPY contracts/ contracts/

RUN cargo build --release

# -------- STAGE 2: RUN --------
FROM debian:buster-slim

# Install needed system libraries
RUN apt-get update && apt-get install -y \
    libssl-dev \
    pkg-config \
    ca-certificates \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Copy compiled binary from builder
COPY --from=builder /app/target/release/rust /app/

# Start the Rust binary
CMD ["./rust"]
