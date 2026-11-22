PHP_PREFIX ?= $(HOME)/.local/share/php-zts
PHP_CONFIG ?= $(if $(wildcard $(PHP_PREFIX)/bin/php-config),$(PHP_PREFIX)/bin/php-config,php-config)
CARGO_TARGET_DIR ?= target/zts
LOCATE_PHP = PREFIX=$$("$(PHP_CONFIG)" --prefix) || exit 1; LIBPHP=$$(find "$$PREFIX/lib64" "$$PREFIX/lib" -maxdepth 1 \( -name libphp.so -o -name libphp.dylib \) 2>/dev/null | head -1); test -n "$$LIBPHP" || { echo "no embed library under $$PREFIX; build PHP with --enable-embed=shared"; exit 1; }; LIBDIR=$$(dirname "$$LIBPHP")
PHP_ENV = PHP_CONFIG="$(PHP_CONFIG)" LD_LIBRARY_PATH="$$LIBDIR$${LD_LIBRARY_PATH:+:$$LD_LIBRARY_PATH}" DYLD_LIBRARY_PATH="$$LIBDIR$${DYLD_LIBRARY_PATH:+:$$DYLD_LIBRARY_PATH}"

GEN_STUB ?= $(firstword $(wildcard $(shell $(PHP_CONFIG) --prefix)/lib*/php/build/gen_stub.php))
PHP_BIN ?= $(shell $(PHP_CONFIG) --prefix)/bin/php

BUF_VERSION ?= v1.73.0
BUF ?= go run github.com/bufbuild/buf/cmd/buf@$(BUF_VERSION)

.PHONY: test test_unit test_e2e coverage stubs grpc_fixtures php

stubs:
	@test -f "$(GEN_STUB)" || { echo "gen_stub.php not found at $(GEN_STUB); set GEN_STUB=/path/to/gen_stub.php"; exit 1; }
	@test -x "$(PHP_BIN)" || { echo "php not found at $(PHP_BIN); set PHP_BIN=/path/to/php"; exit 1; }
	@mkdir -p target/stubgen
	@cp "$(GEN_STUB)" target/stubgen/gen_stub.php
	@for stub in $$(find crates -name '*.stub.php' -not -path '*/target/*'); do \
		$(PHP_BIN) target/stubgen/gen_stub.php "$$stub" || exit 1; \
	done

grpc_fixtures:
	@cd crates/tests && \
	$(BUF) build fixtures/grpc --as-file-descriptor-set -o fixtures/grpc/echo.binpb && \
	$(BUF) build fixtures/grpc --as-file-descriptor-set --exclude-imports -o fixtures/grpc/echo-no-imports.binpb && \
	$(BUF) build fixtures/grpc_options --as-file-descriptor-set --path fixtures/grpc_options/ping.proto --exclude-imports -o fixtures/grpc_options/ping-no-imports.binpb

test:
	@$(MAKE) test_unit
	@$(MAKE) test_e2e

test_unit:
	@$(LOCATE_PHP); \
	RUSTFLAGS="$$RUSTFLAGS --cfg tokio_unstable" CARGO_TARGET_DIR="$(CARGO_TARGET_DIR)" $(PHP_ENV) cargo test --workspace

test_e2e:
	@$(LOCATE_PHP); \
	export RUSTFLAGS="$$RUSTFLAGS --cfg tokio_unstable" && \
	CARGO_TARGET_DIR="$(CARGO_TARGET_DIR)" $(PHP_ENV) cargo build -p rapira_core --bin rapira && \
	CARGO_TARGET_DIR="$(CARGO_TARGET_DIR)" $(PHP_ENV) cargo test -p tests --test e2e --features e2e -- --test-threads=1

PHP_SRC ?= ../../third-party/php-src

php:
	@test -d "$(PHP_SRC)" || { echo "php-src not found at $(PHP_SRC); set PHP_SRC=/path/to/php-src"; exit 1; }
	-@$(MAKE) -C "$(PHP_SRC)" distclean >/dev/null 2>&1
	@FLAGS="$$(tr '\n' ' ' < .github/php-configure-flags.txt)"; \
	cd "$(PHP_SRC)" && \
	./buildconf --force && \
	./configure --prefix="$(PHP_PREFIX)" $$FLAGS
	@$(MAKE) -C "$(PHP_SRC)" -j"$$(getconf _NPROCESSORS_ONLN)"
	@$(MAKE) -C "$(PHP_SRC)" install

# A forced exit skips the profile write. Continuous mode (%c) keeps the counters in a mapped file.
# https://clang.llvm.org/docs/SourceBasedCodeCoverage.html#running-the-instrumented-program
coverage:
	@$(LOCATE_PHP); \
	export CARGO_TARGET_DIR=target/coverage $(PHP_ENV) && \
	export RUSTFLAGS="$$RUSTFLAGS --cfg tokio_unstable -Cllvm-args=-runtime-counter-relocation" && \
	eval "$$(cargo llvm-cov show-env --export-prefix)" && \
	export LLVM_PROFILE_FILE="$$CARGO_LLVM_COV_TARGET_DIR/rapira-%p%c.profraw" && \
	cargo llvm-cov clean --workspace && \
	cargo test --workspace && \
	cargo build -p rapira_core --bin rapira && \
	cargo test -p tests --test e2e --features e2e -- --test-threads=1 && \
	cargo llvm-cov report --workspace --lcov --output-path lcov.info \
		--ignore-filename-regex '(crates/tests/|bindings\.rs$$|/src/main\.rs$$)'
