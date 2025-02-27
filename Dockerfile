FROM rust AS builder

WORKDIR /usr/src/packet-flow
COPY . .

# Build with release optimizations
RUN cargo install --path .

# Runtime stage
FROM alpine:latest

# Install required runtime dependencies
# RUN apk --no-cache add ca-certificates

WORKDIR /app

# Copy the binary from the builder stage
COPY --from=builder /usr/local/cargo/bin/packet-flow .

# Run as non-root user for better security
RUN addgroup -S appuser && adduser -S appuser -G appuser && chown -R appuser:appuser /app
USER appuser

ENTRYPOINT ["/app/packet-flow"]