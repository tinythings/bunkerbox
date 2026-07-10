.PHONY: dev check

dev:
	cargo build

check:
	cargo fmt --all
	cargo clippy --all-targets --all-features -- -D warnings || cargo clippy --fix --all-targets --all-features --allow-dirty --allow-staged -- -D warnings
