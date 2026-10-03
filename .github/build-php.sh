#!/bin/sh
# Builds the PHP embed SAPI from the php.net release tarball, with igbinary and redis, into $PHP_PREFIX.
# Usage: PHP_PREFIX=/path .github/build-php.sh 8.5.1
set -eu

FLAGS=$(tr '\n' ' ' < "$(dirname "$0")/php-configure-flags.txt")
SHA=sha256sum
EXTRA=""
if [ "$(uname)" = Darwin ]; then
    SHA="shasum -a 256"
    export PKG_CONFIG_PATH="$(brew --prefix openssl@3)/lib/pkgconfig:$(brew --prefix curl)/lib/pkgconfig:$(brew --prefix oniguruma)/lib/pkgconfig:$(brew --prefix libxml2)/lib/pkgconfig:$(brew --prefix sqlite)/lib/pkgconfig:$(brew --prefix libffi)/lib/pkgconfig:$(brew --prefix icu4c)/lib/pkgconfig:$(brew --prefix libpq)/lib/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
    EXTRA="--with-iconv=$(xcrun --show-sdk-path)/usr --with-gettext=$(brew --prefix gettext)"
fi

curl -fsSL "https://www.php.net/distributions/php-$1.tar.gz" -o php.tar.gz
tar xf php.tar.gz
cd "php-$1"
curl -fsSL --retry 3 https://pecl.php.net/get/igbinary-3.2.17RC1.tgz -o igbinary.tgz
curl -fsSL --retry 3 https://pecl.php.net/get/redis-6.3.0.tgz -o redis.tgz
echo '91da821443db125282a6aea039f24588dd28ff5d71e8187f6ecc41165bceafbc  igbinary.tgz' | $SHA -c -
echo '0d5141f634bd1db6c1ddcda053d25ecf2c4fc1c395430d534fd3f8d51dd7f0b5  redis.tgz' | $SHA -c -
mkdir -p ext/igbinary ext/redis
tar -xzf igbinary.tgz -C ext/igbinary --strip-components=1
tar -xzf redis.tgz -C ext/redis --strip-components=1
./buildconf --force
export CFLAGS="-O2"
./configure --prefix="$PHP_PREFIX" $FLAGS --enable-igbinary --enable-redis --enable-redis-igbinary $EXTRA
make -j"$(getconf _NPROCESSORS_ONLN)"
make install
