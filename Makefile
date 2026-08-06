-include local.env
export

.PHONY: dev test

dev:
	cargo run

test:
	cargo test --all-targets
