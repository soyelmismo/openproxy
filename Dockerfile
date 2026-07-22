# syntax=docker/dockerfile:1.7
#
# Dockerfile for openproxy using distroless image with pre-built release binaries.
#
# Expects pre-built binaries at:
#   bin/amd64/openproxy
#   bin/arm64/openproxy
#
# Usage (CI / pre-built):
#   docker buildx build --platform linux/amd64,linux/arm64 -t openproxy .
#

FROM gcr.io/distroless/cc:nonroot AS runtime

ARG TARGETARCH

# Copy the pre-built binary for target architecture and example config.
COPY --chown=65532:65532 bin/${TARGETARCH}/openproxy /usr/local/bin/openproxy
COPY --chown=65532:65532 config.example.toml /etc/openproxy/config.example.toml

USER 65532:65532
WORKDIR /var/lib/openproxy

# openproxy binds 127.0.0.1:8787 by default (see config.example.toml).
# When running in Docker the operator overrides `server.bind` to `0.0.0.0:8787`.
EXPOSE 8787

# Persistent state: SQLite database, encryption key file, etc.
VOLUME ["/var/lib/openproxy"]

ENTRYPOINT ["/usr/local/bin/openproxy"]
CMD ["--config", "/etc/openproxy/config.toml"]
