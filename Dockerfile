# syntax=docker/dockerfile:1

ARG PHP_BASE=php:8.5-cli-trixie@sha256:54d82ff9be6bd198145e90c917fc9b2e24230b42e52def8deb3554baf61c451a
ARG RUST_BASE=rust:1-trixie@sha256:b1b3c9c0d921d7fa0a6d1f9ec7e4eab87f8c8ec97644c3d791450f131dec813f

FROM ${RUST_BASE} AS rust

FROM ${PHP_BASE} AS php-extensions

RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends libicu-dev libpq-dev; \
    docker-php-source extract; \
    curl -fsSL --retry 3 https://pecl.php.net/get/igbinary-3.2.17RC1.tgz -o /tmp/igbinary.tgz; \
    curl -fsSL --retry 3 https://pecl.php.net/get/redis-6.3.0.tgz -o /tmp/redis.tgz; \
    echo '91da821443db125282a6aea039f24588dd28ff5d71e8187f6ecc41165bceafbc  /tmp/igbinary.tgz' | sha256sum -c -; \
    echo '0d5141f634bd1db6c1ddcda053d25ecf2c4fc1c395430d534fd3f8d51dd7f0b5  /tmp/redis.tgz' | sha256sum -c -; \
    mkdir -p /usr/src/php/ext/igbinary /usr/src/php/ext/redis; \
    tar -xzf /tmp/igbinary.tgz -C /usr/src/php/ext/igbinary --strip-components=1; \
    tar -xzf /tmp/redis.tgz -C /usr/src/php/ext/redis --strip-components=1; \
    docker-php-ext-install -j"$(nproc)" bcmath intl pdo_pgsql pgsql igbinary; \
    docker-php-ext-configure redis --enable-redis-igbinary; \
    docker-php-ext-install -j"$(nproc)" redis; \
    docker-php-source delete; \
    rm -rf /var/lib/apt/lists/* /tmp/igbinary.tgz /tmp/redis.tgz

FROM php-extensions AS builder
ARG PHP_BASE

COPY --from=rust /usr/local/rustup /usr/local/rustup
COPY --from=rust /usr/local/cargo /usr/local/cargo
ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH

RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends clang libclang-dev; \
    rm -rf /var/lib/apt/lists/*

RUN rustup toolchain install stable --profile minimal --component rustfmt --component clippy

WORKDIR /src
COPY . .

ENV RUSTFLAGS="-C link-arg=-Wl,-rpath,/usr/local/lib"

RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/target,id=rapira-target-${PHP_BASE},sharing=locked \
    set -eux; \
    cargo build --release --locked --bin rapira; \
    cp target/release/rapira /usr/local/bin/rapira; \
    ldd /usr/local/bin/rapira | grep -q '/usr/local/lib/libphp.so'

FROM php-extensions AS payload

COPY --from=builder /usr/local/bin/rapira /out/usr/local/bin/rapira
COPY --from=builder /usr/local/lib/libphp.so /out/usr/local/lib/libphp.so

# PHP 8.4 builds opcache as a shared module and 8.5 links it into libphp, so the copy is conditional.
RUN set -eux; \
    ext_dir="$(php-config --extension-dir)"; \
    for ext in bcmath intl pdo_pgsql pgsql igbinary redis; do \
        install -D "$ext_dir/$ext.so" "/out$ext_dir/$ext.so"; \
        install -D -m 0644 "$PHP_INI_DIR/conf.d/docker-php-ext-$ext.ini" \
                           "/out$PHP_INI_DIR/conf.d/docker-php-ext-$ext.ini"; \
    done; \
    if [ -f "$ext_dir/opcache.so" ]; then \
        install -D "$ext_dir/opcache.so" "/out$ext_dir/opcache.so"; \
        install -D -m 0644 "$PHP_INI_DIR/conf.d/docker-php-ext-opcache.ini" \
                           "/out$PHP_INI_DIR/conf.d/docker-php-ext-opcache.ini"; \
    fi; \
    php --ri "Zend OPcache" > /dev/null

WORKDIR /out/usr/local/share/rapira

# Records the Debian packages owning shared objects the payload needs from outside /usr/local: https://manpages.debian.org/trixie/dpkg/dpkg-query.1.en.html#S
RUN find /out/usr/local -type f \( -executable -o -name '*.so' \) -exec ldd '{}' ';' 2>/dev/null \
        | awk '/=>/ { so = $(NF-1); if (index(so, "/usr/local/") == 1) { next }; gsub("^/(usr/)?", "", so); printf "*%s\n", so }' \
        | sort -u \
        | xargs -r dpkg-query --search \
        | awk 'sub(":$", "", $1) { print $1 }' \
        | sort -u > debian-packages.txt

RUN php -r 'echo PHP_VERSION, "\n";' > PHP_VERSION.txt

# The staged pair links and runs.
RUN /out/usr/local/bin/rapira --version

FROM scratch AS runtime

LABEL org.opencontainers.image.title="Rapira" \
      org.opencontainers.image.description="rapira binary and libphp.so, staged for COPY --from into your own image" \
      org.opencontainers.image.url="https://rapira.rs" \
      org.opencontainers.image.source="https://github.com/rapira-rs/rapira" \
      org.opencontainers.image.licenses="MIT"

COPY --from=payload /out/ /
