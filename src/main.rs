mod abi;
mod app;
mod config;
mod constants;
mod engine;
mod executor;
mod gas;
mod ingest;
mod math;
mod pricing;
mod routing;
mod state;
mod stats;
mod verify;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "robinarb", about = "Rust arbitrage bot on Robinhood Chain")]
struct Cli {
    /// Path to config.toml
    #[arg(long, default_value = "config.toml")]
    config: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the bot (paper mode unless [execution].enabled = true).
    Run,
    /// Validate config + RPC connectivity, print head block and gas oracle params.
    CheckConfig,
    /// Run discovery + hydration, then exit (populates the cache).
    Discover,
    /// Bootstrap, then compare local state vs fresh eth_call for a sample of pools.
    VerifyState {
        #[arg(long, default_value_t = 25)]
        sample: usize,
    },
    /// Find currently model-profitable routes and verify each hop against an
    /// on-chain quote (tells you if the paper opportunities are real or phantom).
    VerifyQuote {
        #[arg(long, default_value_t = 5)]
        top: usize,
    },
    /// Comprehensive per-DEX quote accuracy (model vs on-chain, multiple sizes)
    /// plus local vs on-chain quote latency benchmark.
    VerifyMath,
    /// Benchmark: re-quote every route in the store once, ignoring the
    /// ChangedBatch trigger (as if each block had to scan the whole route
    /// universe instead of only trigger-flagged pools).
    BenchRoutes,
}

#[tokio::main]
async fn main() -> Result<()> {
    // NOTE: no reth-* dependency here (RPC-only, no local MDBX), so there is
    // no conflict between competing rustls crypto backends and no need to
    // install an explicit provider. If a future dependency introduces a
    // second rustls backend, the first symptom is a panic on the first TLS
    // connection; re-add
    // `rustls::crypto::ring::default_provider().install_default()` here then.

    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,robinarb=debug".into()),
        )
        .init();

    let cli = Cli::parse();
    let cfg = config::read_config(&cli.config)?;

    match cli.cmd {
        Cmd::CheckConfig => {
            app::check_config(&cfg).await?;
            verify::check_gas_oracle(&cfg).await
        }
        Cmd::Run => {
            let app = app::App::bootstrap(cfg).await?;
            app.run().await
        }
        Cmd::Discover => {
            let app = app::App::bootstrap(cfg).await?;
            tracing::info!(pools = app.engine.len(), routes = app.store.len(), "discovery + hydration done");
            Ok(())
        }
        Cmd::VerifyState { sample } => {
            let app = app::App::bootstrap(cfg).await?;
            verify::verify_state(&app, sample).await
        }
        Cmd::VerifyQuote { top } => {
            let app = app::App::bootstrap(cfg).await?;
            verify::verify_quotes(&app, top).await
        }
        Cmd::VerifyMath => {
            let app = app::App::bootstrap(cfg).await?;
            verify::verify_math(&app).await
        }
        Cmd::BenchRoutes => {
            let app = app::App::bootstrap(cfg).await?;
            verify::bench_route_scan(&app)
        }
    }
}
