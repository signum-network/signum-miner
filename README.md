# signum-miner

## Features
- windows, linux, macOS, android & more
- x86 32 & 64bit, arm, aarch64 
- direct io
- avx512f, avx2, avx, sse, neon
- opencl

## Binary files and source code releases

https://github.com/signum-network/signum-miner/releases

## Running the binaries

### Config

The miner needs a **config.yaml** file with the following structure:

https://github.com/signum-network/signum-miner/blob/master/config.yaml

### Running

Be sure to have the config file on the same folder of your binary.

For windows, double click on the executable file.
If it refuses to run, start the executable from a command prompt to check for error messages.

For Linux run it with the folliwing command:
```shell
./signum-miner
```

## Build from Sources

 - First you need to install a Rust stable toolchain, check https://www.rust-lang.org/tools/install.
 - Binaries are in **target/debug** or **target/release** depending on optimization.

``` shell
# decide on features to run/build:
simd: support for SSE2, AVX, AVX2 and AVX512F (x86_cpu)
neon: support for Arm NEON (arm_cpu)
opencl: support for OpenCL (gpu)

# for a cpu version with SIMD support:
cargo build --release --features=simd

# for a gpu/cpu version with SIMD support:
cargo build --release --features=opencl,simd

# for a arm cpu version with NEON support:
cargo build --release --features=neon

# for a debug version, just avoid the --release argument:
cargo build
```

## Forked from

This is a code fork from https://github.com/PoC-Consortium/scavenger
