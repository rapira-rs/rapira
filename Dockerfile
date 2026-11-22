# syntax=docker/dockerfile:1

# The base image supplies ZTS PHP and the embed library. https://github.com/rapira-rs/docker
# The workflows pass the PHP image of each minor by digest.
ARG PHP_IMAGE=ghcr.io/rapira-rs/php:8.6-zts

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

# The PHP images have only the extensions of the official PHP image.
# igbinary 3.2.17RC1 supports PHP 8.4 and 8.5.
FROM ${PHP_IMAGE} AS php
RUN set -eux; \
    savedAptMark="$(apt-mark showmanual)"; \
    apt-get update; \
    case "$PHP_VERSION" in \
        8.4.*|8.5.*) \
            apt-get install -y --no-install-recommends libicu-dev libpq-dev; \
            docker-php-source extract; \
            curl -fsSL --retry 3 https://pecl.php.net/get/igbinary-3.2.17RC1.tgz -o /tmp/igbinary.tgz; \
            echo '91da821443db125282a6aea039f24588dd28ff5d71e8187f6ecc41165bceafbc  /tmp/igbinary.tgz' | sha256sum -c -; \
            mkdir -p /usr/src/php/ext/igbinary; \
            tar -xzf /tmp/igbinary.tgz -C /usr/src/php/ext/igbinary --strip-components=1; \
            docker-php-ext-install -j"$(nproc)" bcmath intl pdo_pgsql pgsql igbinary; \
            docker-php-source delete; \
            rm /tmp/igbinary.tgz; \
            ;; \
        8.6.*) \
            apt-get install -y --no-install-recommends libicu-dev libonig-dev libpq-dev; \
            docker-php-ext-install -j"$(nproc)" bcmath intl mbstring pdo_pgsql pgsql; \
            ;; \
        *) \
            echo "no Rapira extension set for PHP $PHP_VERSION" >&2; \
            exit 1; \
            ;; \
    esac; \
    apt-mark auto '.*' > /dev/null; \
    apt-mark manual $savedAptMark > /dev/null; \
    find /usr/local -type f -executable -exec ldd '{}' ';' \
        | awk '/=>/ { so = $(NF-1); if (index(so, "/usr/local/") == 1) { next }; gsub("^/(usr/)?", "", so); printf "*%s\n", so }' \
        | sort -u \
        | xargs -rt dpkg-query --search \
        | awk 'sub(":$", "", $1) { print $1 }' \
        | sort -u \
        | xargs -r apt-mark manual; \
    apt-get purge -y --auto-remove -o APT::AutoRemove::RecommendsImportant=false; \
    apt-get dist-clean

FROM php AS runtime

LABEL org.opencontainers.image.title="Rapira" \
      org.opencontainers.image.description="Rapira with ZTS PHP and PHP extensions" \
      org.opencontainers.image.url="https://rapira.rs" \
      org.opencontainers.image.source="https://github.com/rapira-rs/rapira" \
      org.opencontainers.image.licenses="MIT"

COPY --from=builder /usr/local/bin/rapira /usr/local/bin/rapira
RUN --mount=type=bind,source=.github/test-runtime.sh,target=/tmp/test-runtime.sh \
    sh /tmp/test-runtime.sh /usr/local/bin/rapira "$(php-config --extension-dir)"

CMD ["rapira", "serve", "/app/rapira.toml"]
