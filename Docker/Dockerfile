# Base images are pinned by IMMUTABLE digest (issue #22), same policy as
# Docker/docker-compose.yml: the tag is a readable prefix, the digest is what
# the engine actually resolves. Re-resolve with
# `docker buildx imagetools inspect <image:tag>` when bumping, and bump the
# tag and the digest together.
FROM rust:1.97.1-alpine3.24@sha256:3c38f3f82c2f3d73da3b38e18d279393a04cb43ddded0e35088a8c3324d40900 AS builder

# build-base pulls gcc + musl-dev (the `ring` C sources need them) and make,
# which drives the `make release` target below. curl is REQUIRED: the
# utoipa-swagger-ui build script shells out to it to download the Swagger UI
# bundle, and panics with "`curl` command not found" otherwise. No
# fontconfig/freetype stack: the dependency tree carries no native image or
# font libraries.
RUN apk add --no-cache build-base curl

WORKDIR /app

COPY . .

# The rust:alpine host target is x86_64-unknown-linux-musl, which links
# statically by default, so the release binary is self-contained.
RUN make release

FROM alpine:3.24@sha256:28bd5fe8b56d1bd048e5babf5b10710ebe0bae67db86916198a6eec434943f8b

# Root certificates for the outbound TLS connections (ClickHouse, MongoDB,
# Redis when configured over TLS).
RUN apk add --no-cache ca-certificates

WORKDIR /app

COPY --from=builder /app/target/release/optionchain_simulator /app/

# DEFAULT_PORT in src/infrastructure/config/server.rs. This is metadata, not a
# binding: the compose file publishes ${OCS_PORT} explicitly, but a bare
# `docker run -P` publishes what is declared here, so a custom OCS_PORT needs an
# explicit `-p`.
EXPOSE 7070

CMD ["/app/optionchain_simulator"]
