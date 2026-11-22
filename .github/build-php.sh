#!/bin/sh
# Run in the PHP source directory. PHP_PREFIX is the install directory.
set -eu
minor=$1
model=${2:-zts}
set -- --prefix="$PHP_PREFIX"
while IFS= read -r flag; do
    if [ "$model" = nts ] && [ "$flag" = --enable-zts ]; then flag=--disable-zts; fi
    set -- "$@" "$flag"
done < "$(dirname "$0")/php-configure-flags.txt"
case "$minor" in
    8.4|8.5)
        url=https://pecl.php.net/get/igbinary-3.2.17RC1.tgz
        sha=91da821443db125282a6aea039f24588dd28ff5d71e8187f6ecc41165bceafbc ;;
    8.6) url= ;;
    master)
        url=https://codeload.github.com/igbinary/igbinary/tar.gz/2b1f5d62e3037b07c2a46499ac32091cdb41fb40
        sha=1664c973d836ae7f0dfa92dbbc04504d506f8caf7c14db27e245646aaaceeab8
        set -- "$@" --enable-debug --enable-debug-assertions --enable-zend-test ;;
    *) exit 1 ;;
esac
if [ -n "$url" ]; then
    curl -fsSL --retry 3 "$url" -o igbinary.tgz
    if [ "$(uname -s)" = Darwin ]; then
        echo "$sha  igbinary.tgz" | shasum -a 256 -c -
    else
        echo "$sha  igbinary.tgz" | sha256sum -c -
    fi
    mkdir -p ext/igbinary
    tar -xzf igbinary.tgz -C ext/igbinary --strip-components=1
    set -- "$@" --enable-igbinary
fi
if [ "$(uname -s)" = Darwin ]; then
    export PATH="$(brew --prefix bison)/bin:$PATH"
    for lib in openssl@3 curl oniguruma libxml2 sqlite libffi icu4c libpq libsodium; do
        PKG_CONFIG_PATH="$(brew --prefix "$lib")/lib/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
    done
    export PKG_CONFIG_PATH
    set -- "$@" --with-iconv="$(xcrun --show-sdk-path)/usr" --with-gettext="$(brew --prefix gettext)"
fi
./buildconf --force
./configure "$@"
make -j"$(getconf _NPROCESSORS_ONLN)"
make install
