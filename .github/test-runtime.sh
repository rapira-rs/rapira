#!/bin/sh
# Exercise the packaged interpreter through the server. --version does not start PHP.
set -eu
dir=$(mktemp -d)
pid=
cleanup() {
    if [ -n "$pid" ]; then kill "$pid" 2>/dev/null || :; wait "$pid" || :; fi
    cat "$dir/server.log" >&2
    rm -rf "$dir"
}
trap cleanup EXIT
cat > "$dir/rapira.toml" <<EOF
[http]
listen = "unix:$dir/http.sock"
[http.pool]
entrypoint = "worker.php"
mode = "worker"
workers = 2
EOF
cat > "$dir/worker.php" <<'PHP'
<?php
$handler = static function (): void {
    echo PHP_ZTS && opcache_get_status(false)['opcache_enabled'] ? 'zts-opcache' : 'unsupported';
};
while (\Rapira\handle_request($handler)) {}
PHP
echo 'opcache.enable=1' > "$dir/php.ini"
if [ -f "$2/opcache.so" ]; then echo "zend_extension=$2/opcache.so" >> "$dir/php.ini"; fi
mkdir "$dir/scan"
PHPRC="$dir/php.ini" PHP_INI_SCAN_DIR="$dir/scan" "$1" serve "$dir/rapira.toml" > "$dir/server.log" 2>&1 &
pid=$!
if ! body=$(curl -fsS --unix-socket "$dir/http.sock" --retry 5 --retry-all-errors --retry-delay 1 --max-time 5 http://localhost/ 2>"$dir/client.log"); then
    cat "$dir/client.log" >&2
    exit 1
fi
if [ "$body" != zts-opcache ]; then
    echo "Expected a ZTS worker with OPcache enabled; received '$body'." >&2
    exit 1
fi
kill "$pid"
wait "$pid"
pid=
