//! Modernized main.rs with async/await support
#![warn(unused_extern_crates)]

#[macro_use]
extern crate serde_derive;
#[macro_use]
extern crate cfg_if;
#[macro_use]
extern crate log;

mod com;
mod config;
mod cpu_worker;
mod future;
mod logger;
mod miner;
mod plot;
mod poc_hashing;
mod reader;
mod requests;
mod shabal256;
mod utils;

#[cfg(feature = "opencl")]
mod gpu_worker;
#[cfg(feature = "opencl")]
mod ocl;

use crate::config::load_cfg;
use crate::miner::Miner;
use clap::{Arg, Command};
use std::process;

cfg_if! {
    if #[cfg(feature = "simd_avx512f")] {
        extern "C" {
            pub fn init_shabal_avx512f();
        }

        fn init_cpu_extensions() {
            info!("SIMD extensions: AVX512F");
            unsafe { init_shabal_avx512f(); }
        }
    } else if #[cfg(feature = "simd_avx2")] {
        extern "C" {
            pub fn init_shabal_avx2();
        }

        fn init_cpu_extensions() {
            info!("SIMD extensions: AVX2");
            unsafe { init_shabal_avx2(); }
        }
    } else if #[cfg(feature = "simd_avx")] {
        extern "C" {
            pub fn init_shabal_avx();
        }

        fn init_cpu_extensions() {
            info!("SIMD extensions: AVX");
            unsafe { init_shabal_avx(); }
        }
    } else if #[cfg(feature = "simd_sse2")] {
        extern "C" {
            pub fn init_shabal_sse2();
        }

        fn init_cpu_extensions() {
            info!("SIMD extensions: SSE2");
            unsafe { init_shabal_sse2(); }
        }
    }  else if #[cfg(feature = "neon")] {
         extern "C" {
            pub fn init_shabal_neon();
        }
        fn init_cpu_extensions() {
            info!("SIMD extensions: neon");
            unsafe { init_shabal_neon();}
        }
    } else {
        fn init_cpu_extensions() {
            info!("SIMD extensions: none");
        }
    }
}
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
fn print_simd_support() {
    println!("SIMD support check:");

    if std::is_x86_feature_detected!("avx512f") {
        println!("✅ AVX-512F supported");
    }
    if std::is_x86_feature_detected!("avx2") {
        println!("✅ AVX2 supported");
    }
    if std::is_x86_feature_detected!("avx") {
        println!("✅ AVX supported");
    }
    if std::is_x86_feature_detected!("sse4.2") {
        println!("✅ SSE4.2 supported");
    }
    if std::is_x86_feature_detected!("sse4.1") {
        println!("✅ SSE4.1 supported");
    }
    if std::is_x86_feature_detected!("sse2") {
        println!("✅ SSE2 supported");
    }
}


#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let cmd = Command::new(env!("CARGO_PKG_NAME"))
        .version(env!("CARGO_PKG_VERSION"))
        .author(env!("CARGO_PKG_AUTHORS"))
        .about(env!("CARGO_PKG_DESCRIPTION"))
        .arg(
            Arg::new("config")
                .short('c')
                .long("config")
                .value_name("FILE")
                .help("Location of the config file")
                .default_value("config.yaml")
                .required(false),
        );

    #[cfg(feature = "opencl")]
    let cmd = cmd.arg(
        Arg::new("opencl")
            .short('o')
            .long("opencl")
            .help("Display OpenCL platforms and devices")
            .action(clap::ArgAction::SetTrue),
    );

    let matches = cmd.get_matches();
    let config = matches
        .get_one::<String>("config")
        .map(|s| s.as_str())
        .unwrap_or("config.yaml");

    let cfg_loaded = load_cfg(config);
    logger::init_logger(&cfg_loaded);

    info!(
        "{} v{}",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION")
    );
    
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    print_simd_support();

    #[cfg(feature = "opencl")]
    info!("GPU extensions: OpenCL");

    #[cfg(feature = "opencl")]
    if matches.get_flag("opencl") {
        ocl::platform_info();
        process::exit(0);
    }

    #[cfg(any(
        feature = "simd_avx512f",
        feature = "simd_avx2",
        feature = "simd_avx",
        feature = "simd_sse2",
        feature = "neon"
    ))]
    init_cpu_extensions();

    #[cfg(feature = "opencl")]
    ocl::gpu_info(&cfg_loaded);

    let handle = tokio::runtime::Handle::current();
    let miner = Miner::new(cfg_loaded, handle);
    miner.run().await;
}
