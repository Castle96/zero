# Stage 1: Build a fully static Rust binary using musl
FROM rust:1-alpine3.21 AS builder

RUN apk add --no-cache musl-dev
RUN rustup target add x86_64-unknown-linux-musl

WORKDIR /app
COPY . .

# Build with musl target for a fully self-contained binary
RUN cargo build --release --target x86_64-unknown-linux-musl --bin http_server

# Stage 2: Minimal runtime image
FROM alpine:3.21

# CA certificates (for upstream HTTPS) and timezone data (for chrono cert dates)
RUN apk add --no-cache ca-certificates tzdata

COPY --from=builder \
    /app/target/x86_64-unknown-linux-musl/release/http_server \
    /http_server

EXPOSE 80 443

# Config file path — mount a ConfigMap here
ENV CONFIG_PATH=/etc/http-server/config.toml

ENTRYPOINT ["/http_server"]
