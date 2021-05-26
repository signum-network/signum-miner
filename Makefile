start:
	cargo build && target/debug/signum-miner
test:
	cargo test
debug:
	cargo build
release:
	cargo build --release
release-gpu:
	cargo build --release --features=opencl
