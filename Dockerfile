# syntax=docker/dockerfile:1

# The Rapira image: rapira on an NTS PHP image from dockerfiles/<minor>/nts.
# The workflows pass the PHP image of each minor by digest.
ARG PHP_IMAGE=ghcr.io/rapira-rs/php:8.6-nts

# The tag moves to each stable Rust release, as rust-toolchain.toml and the CI workflows do.
FROM rust:1-trixie AS rust

FROM ${PHP_IMAGE} AS builder

COPY --from=rust /usr/local/rustup /usr/local/rustup
COPY --from=rust /usr/local/cargo /usr/local/cargo
ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH

RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends clang libclang-dev; \
    rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY . .

ENV RUSTFLAGS="--cfg tokio_unstable -C link-arg=-Wl,-rpath,/usr/local/lib"

RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    set -eux; \
    cargo build --release --locked --bin rapira; \
    cp target/release/rapira /usr/local/bin/rapira; \
    ldd /usr/local/bin/rapira | grep -q '/usr/local/lib/libphp.so'

FROM ${PHP_IMAGE} AS runtime

LABEL org.opencontainers.image.title="Rapira" \
      org.opencontainers.image.description="Rapira with NTS PHP and PHP extensions" \
      org.opencontainers.image.url="https://rapira.rs" \
      org.opencontainers.image.source="https://github.com/rapira-rs/rapira" \
      org.opencontainers.image.licenses="MIT"

COPY --from=builder /usr/local/bin/rapira /usr/local/bin/rapira
RUN rapira --version

CMD ["rapira", "serve", "/app/rapira.toml"]
