# syntax=docker/dockerfile:1

# ghcr.io/rapira-rs/php:8.4-zts: ZTS PHP 8.4 with PHP extensions.
# The build context is the dockerfiles directory:
#   docker build -f dockerfiles/8.4/zts/Dockerfile --build-arg PHP_VERSION=8.4.<patch> dockerfiles
# PHP_VERSION is 8.4.28 or a newer php-src release of PHP 8.4.

# The source tarball, made from the signed php-src tag.
FROM debian:trixie-slim AS php-source
# The release managers of PHP 8.4: the GPG_KEYS line of 8.4 in https://github.com/docker-library/php
ENV GPG_KEYS="AFD8691FDAEDF03BDF6E460563F15A9B715376CA 9D7F99A0CB8F05C8A6958D6256A97AF7600A39A6 0616E93D95AF471243E26761770426E17EBBB3DD"
ARG PHP_VERSION
RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends \
        autoconf bison bzip2 ca-certificates dirmngr git gpg gpg-agent make re2c xz-utils; \
    git init -q /src; \
    cd /src; \
    git fetch --depth 1 https://github.com/php/php-src.git "refs/tags/php-$PHP_VERSION:refs/tags/php-$PHP_VERSION"; \
    export GNUPGHOME="$(mktemp -d)"; \
    for key in $GPG_KEYS; do \
        gpg --batch --keyserver keyserver.ubuntu.com --recv-keys "$key"; \
    done; \
    git verify-tag "php-$PHP_VERSION"; \
    git checkout -q "php-$PHP_VERSION"; \
    scripts/dev/makedist "php-$PHP_VERSION"; \
    mv "php-$PHP_VERSION.tar.xz" /php.tar.xz

# PHP, built with the 8.4/trixie/zts recipe of https://github.com/docker-library/php.
FROM debian:trixie-slim

# APT must not install the PHP packages of Debian.
RUN set -eux; \
    { \
        echo 'Package: php*'; \
        echo 'Pin: release *'; \
        echo 'Pin-Priority: -1'; \
    } > /etc/apt/preferences.d/no-debian-php

# The packages that phpize needs.
ENV PHPIZE_DEPS="autoconf dpkg-dev file g++ gcc libc-dev make pkg-config re2c"

RUN set -eux; \
    apt-get update; \
    apt-get upgrade -y; \
    apt-get install -y --no-install-recommends $PHPIZE_DEPS ca-certificates curl xz-utils; \
    apt-get dist-clean

ARG PHP_VERSION
ENV PHP_VERSION=$PHP_VERSION
ENV PHP_INI_DIR=/usr/local/etc/php
ENV PHP_CFLAGS="-fstack-protector-strong -fpic -fpie -O2 -D_LARGEFILE_SOURCE -D_FILE_OFFSET_BITS=64"
ENV PHP_CPPFLAGS="$PHP_CFLAGS"
ENV PHP_LDFLAGS="-Wl,-O1 -pie"

COPY --from=php-source /php.tar.xz /usr/src/php.tar.xz
COPY scripts/docker-php-* /usr/local/bin/
COPY scripts/LICENSE /usr/local/share/doc/docker-library-php/LICENSE

# After the build, apt keeps the packages of the libraries that the files in /usr/local link, and removes the other build packages.
RUN set -eux; \
    savedAptMark="$(apt-mark showmanual)"; \
    apt-get update; \
    apt-get install -y --no-install-recommends \
        libargon2-dev \
        libcurl4-openssl-dev \
        libonig-dev \
        libreadline-dev \
        libsodium-dev \
        libsqlite3-dev \
        libssl-dev \
        libxml2-dev \
        zlib1g-dev; \
    export \
        CFLAGS="$PHP_CFLAGS" \
        CPPFLAGS="$PHP_CPPFLAGS" \
        LDFLAGS="$PHP_LDFLAGS" \
        PHP_BUILD_PROVIDER='https://github.com/rapira-rs/rapira' \
        PHP_UNAME='Linux - Docker'; \
    mkdir -p "$PHP_INI_DIR/conf.d"; \
    docker-php-source extract; \
    cd /usr/src/php; \
    gnuArch="$(dpkg-architecture --query DEB_BUILD_GNU_TYPE)"; \
    debMultiarch="$(dpkg-architecture --query DEB_BUILD_MULTIARCH)"; \
    if [ ! -d /usr/include/curl ]; then \
        ln -sT "/usr/include/$debMultiarch/curl" /usr/local/include/curl; \
    fi; \
    ./configure \
        --build="$gnuArch" \
        --sysconfdir="${PHP_INI_DIR%/php}" \
        --with-config-file-path="$PHP_INI_DIR" \
        --with-config-file-scan-dir="$PHP_INI_DIR/conf.d" \
        --enable-option-checking=fatal \
        --with-mhash \
        --with-pic \
        --enable-mbstring \
        --enable-mysqlnd \
        --with-password-argon2 \
        --with-sodium=shared \
        --with-pdo-sqlite=/usr \
        --with-sqlite3=/usr \
        --with-curl \
        --with-iconv \
        --with-openssl \
        --with-readline \
        --with-zlib \
        --enable-phpdbg \
        --enable-phpdbg-readline \
        --with-pear \
        --with-libdir="lib/$debMultiarch" \
        --enable-embed \
        --enable-zts \
        --disable-zend-signals; \
    make -j "$(nproc)"; \
    find -type f -name '*.a' -delete; \
    make install; \
    find /usr/local -type f -perm '/0111' -exec sh -euxc 'strip --strip-all "$@" || :' -- '{}' +; \
    make clean; \
    cp -v php.ini-* "$PHP_INI_DIR/"; \
    cd /; \
    docker-php-source delete; \
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
    apt-get dist-clean; \
    pecl update-channels; \
    rm -rf /tmp/pear ~/.pearrc; \
    docker-php-ext-enable sodium opcache; \
    php --version

# The PHP extensions of the Rapira image, without redis: redis 6.3.0 is not safe on ZTS PHP (https://github.com/phpredis/phpredis/issues/2976).
RUN set -eux; \
    savedAptMark="$(apt-mark showmanual)"; \
    apt-get update; \
    apt-get install -y --no-install-recommends libicu-dev libpq-dev; \
    docker-php-source extract; \
    curl -fsSL --retry 3 https://pecl.php.net/get/igbinary-3.2.17RC1.tgz -o /tmp/igbinary.tgz; \
    echo '91da821443db125282a6aea039f24588dd28ff5d71e8187f6ecc41165bceafbc  /tmp/igbinary.tgz' | sha256sum -c -; \
    mkdir -p /usr/src/php/ext/igbinary; \
    tar -xzf /tmp/igbinary.tgz -C /usr/src/php/ext/igbinary --strip-components=1; \
    docker-php-ext-install -j"$(nproc)" bcmath intl pdo_pgsql pgsql igbinary; \
    docker-php-source delete; \
    rm /tmp/igbinary.tgz; \
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
    apt-get dist-clean; \
    php --ri "Zend OPcache" > /dev/null

LABEL org.opencontainers.image.title="PHP" \
      org.opencontainers.image.description="ZTS PHP 8.4 with PHP extensions" \
      org.opencontainers.image.url="https://rapira.rs" \
      org.opencontainers.image.source="https://github.com/rapira-rs/rapira"
ENTRYPOINT ["docker-php-entrypoint"]
CMD ["php", "-a"]
