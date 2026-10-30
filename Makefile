.PHONY: build test lint fmt clean

build:
	cargo build --release --locked

test:
	cargo test --locked

lint:
	cargo fmt --all -- --check
	cargo clippy --all-targets --locked -- -D warnings

fmt:
	cargo fmt --all

clean:
	cargo clean
