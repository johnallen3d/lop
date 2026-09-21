.PHONY: check fmt test lint

check: fmt test lint

fmt:
	cargo fmt --check

test:
	cargo test

lint:
	cargo clippy --all-targets --all-features -- -D warnings -D clippy::pedantic
