# Stage 1 - Build the app
FROM rust:1.76 as builder

# Set working directory inside the container
WORKDIR /app

# Cache dependencies
COPY Cargo.toml .
COPY src/ src/
RUN cargo build --release

# Stage 2 - Runtime image
FROM debian:buster-slim

# Install needed system libs
RUN apt-get update && apt-get install -y libssl-dev ca-certificates && rm -rf /var/lib/apt/lists/*

# Set working directory
WORKDIR /app

# Copy compiled binary from the build stage
COPY --from=builder /app/target/release/rust /app/

# Run the binary
CMD ["./rust"]
