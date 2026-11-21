# syntax=docker/dockerfile:1

# Static (musl) sms-gateway binary in a distroless, non-root image.
#
# The build stage always runs on the build host's architecture and
# cross-compiles with cargo-zigbuild (zig as C compiler and linker, for
# ring's C/assembly), so a multi-arch build needs no QEMU:
#
#   docker buildx build -f Containerfile --platform linux/amd64,linux/arm64 \
#     -t registry.example/sms-gateway --push .
#   podman build --platform linux/amd64,linux/arm64 --manifest sms-gateway -f Containerfile .
#
#   docker run --rm -p 8080:8080 \
#     -e SMS_GATEWAY__DATABASE__URL=postgres://... \
#     -e SMS_GATEWAY__MODEM__KIND=tplink \
#     -e SMS_GATEWAY__MODEM__TPLINK__URL=http://192.168.254.1 \
#     -e SMS_GATEWAY__MODEM__TPLINK__PASSWORD=... \
#     sms-gateway            # default command: serve
#
# A config file is optional: ./config.toml is read when present, i.e.
# mount it at /etc/sms-gateway/config.toml (the working directory).

ARG RUST_VERSION=1.100
ARG DEBIAN_RELEASE=trixie

FROM --platform=$BUILDPLATFORM docker.io/library/rust:${RUST_VERSION}-slim-${DEBIAN_RELEASE} AS build

ARG ZIG_VERSION=0.17.1
ARG ZIG_SHA256_AMD64=3581863e3cbcfd0fce0efc682ce7e9e93da96668a977791d356a26d2a7e9b17a
ARG ZIG_SHA256_ARM64=a1546d5646eb184437918d6635ca6b762a0e889eac09cef720ee7a1ed3fe7795
ARG CARGO_ZIGBUILD_VERSION=0.24.1
ARG BUILDARCH

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl xz-utils \
    && rm -rf /var/lib/apt/lists/*

RUN set -eux; \
    case "$BUILDARCH" in \
        amd64) zig_arch=x86_64;  zig_sha256="$ZIG_SHA256_AMD64" ;; \
        arm64) zig_arch=aarch64; zig_sha256="$ZIG_SHA256_ARM64" ;; \
        *) echo "unsupported build platform: $BUILDARCH" >&2; exit 1 ;; \
    esac; \
    curl -fsSL -o /tmp/zig.tar.xz \
        "https://ziglang.org/download/${ZIG_VERSION}/zig-${zig_arch}-linux-${ZIG_VERSION}.tar.xz"; \
    echo "${zig_sha256}  /tmp/zig.tar.xz" | sha256sum -c -; \
    mkdir -p /opt/zig; \
    tar -xJf /tmp/zig.tar.xz -C /opt/zig --strip-components=1; \
    rm /tmp/zig.tar.xz; \
    ln -s /opt/zig/zig /usr/local/bin/zig; \
    zig version

RUN cargo install --locked "cargo-zigbuild@${CARGO_ZIGBUILD_VERSION}" \
    && rm -rf /usr/local/cargo/registry \
    && rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates crates

# The target/ cache is shared by all platforms: cargo serializes concurrent
# builds on its lock and reuses host artifacts (build scripts, proc-macros).
ARG TARGETARCH
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    set -eux; \
    case "$TARGETARCH" in \
        amd64) target=x86_64-unknown-linux-musl ;; \
        arm64) target=aarch64-unknown-linux-musl ;; \
        *) echo "unsupported target platform: $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    cargo zigbuild --release --locked -p gateway --target "$target"; \
    install -D -m 0755 "target/$target/release/sms-gateway" /out/sms-gateway

FROM gcr.io/distroless/static-debian13:nonroot@sha256:aafe993665bf6200db38a02c11a9351e782a84b6a45e7f33b6695638c88e3931

LABEL org.opencontainers.image.title="sms-api-gateway" \
      org.opencontainers.image.description="HTTP SMS gateway in front of a TP-Link NE200-Outdoor 5G CPE, with PostgreSQL storage" \
      org.opencontainers.image.source="https://github.com/alcelafranque/sms-api-gateway" \
      org.opencontainers.image.url="https://github.com/alcelafranque/sms-api-gateway" \
      org.opencontainers.image.documentation="https://github.com/alcelafranque/sms-api-gateway/blob/main/docs/deployment.md" \
      org.opencontainers.image.licenses="GPL-3.0-or-later"

COPY --from=build /out/sms-gateway /usr/local/bin/sms-gateway
COPY config.example.toml /etc/sms-gateway/config.example.toml
# ./config.toml is loaded when present: mount it here.
WORKDIR /etc/sms-gateway
# distroless "nonroot", numeric so that runAsNonRoot can verify it.
USER 65532:65532
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/sms-gateway"]
CMD ["serve"]
