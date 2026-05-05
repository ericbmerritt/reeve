default:
    @just --list

validate: build check-format lint test

build:
    cargo build --workspace

[parallel]
check-format: check-for-trailing-whitespace check-format-rust check-format-nix check-format-md

check-format-rust:
    cargo fmt --all -- --check

check-format-nix:
    rg --files -g '*.nix' -g '!.*' | xargs alejandra -c

check-format-md:
    prettier --check '**/*.md'

check-for-trailing-whitespace:
    ! rg '\s+$' --glob '!Cargo.lock' --glob '!specs/**' --glob '!target/**'

[parallel]
lint: lint-rust lint-deps lint-nix

lint-rust:
    cargo clippy --workspace --all-targets --all-features -- -D warnings

lint-deps:
    cargo deny check

lint-nix:
    rg --files -g '*.nix' -g '!.*' | xargs -L 1 statix check --

test:
    cargo llvm-cov nextest --workspace --fail-under-lines 90 \
      --ignore-filename-regex '(^|/)main\.rs$|reeve-runtime/src/keychain/(macos|linux)\.rs$|reeve-cli/src/keychain\.rs$'

[parallel]
format: remove-trailing-whitespace format-rust format-nix format-md

format-rust:
    cargo fmt --all

format-nix:
    rg --files -g '*.nix' -g '!.*' | xargs alejandra

format-md:
    prettier --write '**/*.md'

remove-trailing-whitespace:
    files=$(rg -l "\s+$" --glob '!Cargo.lock' --glob '!specs/**' --glob '!target/**' || true); \
    if [ -n "$files" ]; then \
        echo "$files" | xargs sed -i "s/[[:space:]]\+$//"; \
    fi
