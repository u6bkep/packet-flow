FROM rust:alpine AS builder

# Install required build dependencies
RUN apk --no-cache add musl-dev

WORKDIR /usr/src/packet-flow
COPY . .

# Build with release optimizations
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/usr/src/packet-flow/target \
    cargo install --path .

# Runtime stage
FROM alpine:latest

WORKDIR /app

# Copy the binary from the builder stage
COPY --from=builder /usr/local/cargo/bin/packet-flow .

# Run as non-root user for better security
RUN addgroup -S appuser && adduser -S appuser -G appuser && chown -R appuser:appuser /app
USER appuser

CMD ["/app/packet-flow"]