# Build stage
FROM rust:1.82-slim AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock* ./
# Cache dependencies
RUN mkdir src && echo "fn main(){}" > src/main.rs && cargo build --release && rm -rf src
COPY src ./src
RUN touch src/main.rs && cargo build --release

# Runtime stage
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/gemini-proxy-server /usr/local/bin/
EXPOSE 8080
CMD ["gemini-proxy-server"]
