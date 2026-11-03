.PHONY: build build-gui test lint fmt clean

build:
	cargo build --release --locked

# The native download manager (needs the `gui` feature; the CLI build stays lean without it).
build-gui:
	cargo build --release --locked --features gui --bins

test:
	cargo test --locked

lint:
	cargo fmt --all -- --check
	cargo clippy --all-targets --all-features --locked -- -D warnings

fmt:
	cargo fmt --all

clean:
	cargo clean
