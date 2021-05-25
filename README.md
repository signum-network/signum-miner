# signum-miner

### Features
- windows, linux, macOS, android & more
- x86 32 & 64bit, arm, aarch64 
- direct io
- avx512f, avx2, avx, sse, neon
- opencl

### Binary + source code releases

https://github.com/signum-network/signum-miner/releases

signum-miner can also be installed directly via cargo:

``` shell
cargo install signum-miner
```

### Development Requirements
- new version of rust, stable toolchain

### Compile, test, ...

Binaries are in **target/debug** or **target/release** depending on optimization.

``` shell
# decide on features to run/build:
simd: support for SSE2, AVX, AVX2 and AVX512F (x86_cpu)
neon: support for Arm NEON (arm_cpu)
opencl: support for OpenCL (gpu)

# build debug und run directly
e.g. cargo run --features=simd    #for a cpu version with SIMD support

# build debug (unoptimized)
e.g cargo build --features=neon   #for a arm cpu version with NEON support

# build release (optimized)
e.g. cargo build --release --features=opencl,simd    #for a cpu/gpu version
```

### Run

```shell
signum-miner --help
```

### Config

The miner needs a **config.yaml** file with the following structure:

https://github.com/signum-network/signum-miner/blob/master/config.yaml

### Forked from

This is a code fork from https://github.com/PoC-Consortium/scavenger
