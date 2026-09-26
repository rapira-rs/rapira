PHP_CONFIG ?= php-config
LOCATE_PHP = PREFIX=$$($(PHP_CONFIG) --prefix 2>/dev/null); test -n "$$PREFIX" || { echo "$(PHP_CONFIG) not found; set PHP_CONFIG=/path/to/php-config"; exit 1; }; LIBPHP=$$(find "$$PREFIX/lib64" "$$PREFIX/lib" "$$PREFIX"/lib/php* -maxdepth 1 \( -name 'libphp*.so' -o -name 'libphp*.dylib' \) 2>/dev/null | head -1); test -n "$$LIBPHP" || { echo "no libphp*.so/.dylib under $$PREFIX; install your distro's PHP embed package or build PHP with --enable-embed=shared"; exit 1; }; LIBDIR=$$(dirname "$$LIBPHP"); mkdir -p target/phplib || exit 1; case "$$LIBPHP" in *.dylib) ln -sf "$$LIBPHP" target/phplib/libphp.dylib || exit 1;; *) ln -sf "$$LIBPHP" target/phplib/libphp.so || exit 1;; esac; PHPLIB="$$PWD/target/phplib"
PHP_ENV = PHP_CONFIG=$(PHP_CONFIG) LD_LIBRARY_PATH="$$PHPLIB:$$LIBDIR" DYLD_LIBRARY_PATH="$$PHPLIB:$$LIBDIR" RUSTFLAGS="-L native=$$PHPLIB"

GEN_STUB ?= $(shell $(PHP_CONFIG) --prefix)/lib/php/build/gen_stub.php
PHP_BIN ?= $(shell $(PHP_CONFIG) --prefix)/bin/php

BUF_VERSION ?= v1.73.0
BUF ?= go run github.com/bufbuild/buf/cmd/buf@$(BUF_VERSION)

.PHONY: test test_nts test_e2e coverage stubs grpc_fixtures php php-macos

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
	@$(MAKE) test_nts
	@$(MAKE) test_e2e

test_nts:
	@$(LOCATE_PHP); \
	CARGO_TARGET_DIR=target/nts $(PHP_ENV) cargo test --workspace

test_e2e:
	@$(LOCATE_PHP); \
	CARGO_TARGET_DIR=target/nts $(PHP_ENV) cargo build -p rapira_core --bin rapira && \
	CARGO_TARGET_DIR=target/nts $(PHP_ENV) cargo test -p tests --test e2e --features e2e -- --test-threads=1

PHP_SRC ?= ../../third-party/php-src
PHP_PREFIX ?= $(HOME)/.local/share/php-nts

php:
	@test -d "$(PHP_SRC)" || { echo "php-src not found at $(PHP_SRC); set PHP_SRC=/path/to/php-src"; exit 1; }
	-@$(MAKE) -C "$(PHP_SRC)" distclean >/dev/null 2>&1
	@FLAGS="$$(tr '\n' ' ' < .github/php-configure-flags.txt)"; \
	cd "$(PHP_SRC)" && \
	./buildconf --force && \
	./configure --prefix="$(PHP_PREFIX)" $$FLAGS
	@$(MAKE) -C "$(PHP_SRC)" -j"$$(getconf _NPROCESSORS_ONLN)"
	@$(MAKE) -C "$(PHP_SRC)" install

php-macos:
	@test -d "$(PHP_SRC)" || { echo "php-src not found at $(PHP_SRC); set PHP_SRC=/path/to/php-src"; exit 1; }
	-@$(MAKE) -C "$(PHP_SRC)" distclean >/dev/null 2>&1
	@FLAGS="$$(tr '\n' ' ' < .github/php-configure-flags.txt)"; \
	export PKG_CONFIG_PATH="$$(brew --prefix openssl@3)/lib/pkgconfig:$$(brew --prefix curl)/lib/pkgconfig:$$(brew --prefix oniguruma)/lib/pkgconfig:$$(brew --prefix libxml2)/lib/pkgconfig:$$(brew --prefix sqlite)/lib/pkgconfig:$$(brew --prefix libffi)/lib/pkgconfig:$$(brew --prefix icu4c)/lib/pkgconfig:$$(brew --prefix libpq)/lib/pkgconfig$${PKG_CONFIG_PATH:+:$$PKG_CONFIG_PATH}"; \
	cd "$(PHP_SRC)" && \
	./buildconf --force && \
	./configure --prefix="$(PHP_PREFIX)" $$FLAGS \
		--with-iconv="$$(xcrun --show-sdk-path)/usr" \
		--with-gettext="$$(brew --prefix gettext)"
	@$(MAKE) -C "$(PHP_SRC)" -j"$$(getconf _NPROCESSORS_ONLN)"
	@$(MAKE) -C "$(PHP_SRC)" install

coverage:
	@$(LOCATE_PHP); \
	export CARGO_TARGET_DIR=target/coverage $(PHP_ENV) && \
	export RUSTFLAGS="$$RUSTFLAGS -Cllvm-args=-runtime-counter-relocation" && \
	eval "$$(cargo llvm-cov show-env --export-prefix)" && \
	export LLVM_PROFILE_FILE="$$CARGO_LLVM_COV_TARGET_DIR/rapira-%p%c.profraw" && \
	cargo llvm-cov clean --workspace && \
	cargo test --workspace && \
	cargo build -p rapira_core --bin rapira && \
	cargo test -p tests --test e2e --features e2e -- --test-threads=1 && \
	cargo llvm-cov report --workspace --lcov --output-path lcov.info \
		--ignore-filename-regex '(crates/tests/|bindings\.rs$$|/src/main\.rs$$)'
