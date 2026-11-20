//! Release-profile faucet refill helper for the full-stack e2e.
//!
//! The faucet PoW grind must outrun the checkpoint freshness window
//! (~60–90 s at testnet difficulty 3000), which a debug-profile grinder
//! cannot do. CI builds this binary with `cargo build --release` and the
//! e2e preflight invokes it when the funded key's balance is low.
//!
//! ```text
//! faucet-refill --rpc <url> --recipient <addr> [--faucet <url>]
//!               [--min-mist <u64>] [--attempts <n>] [--margin <n>]
//! ```

use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};

use podseq_e2e::faucet;

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    let mut rpc = "https://fullnode.testnet.sui.io:443".to_string();
    let mut faucet = faucet::TESTNET_FAUCET.to_string();
    let mut recipient = String::new();
    let mut min_mist = 100_000_000_u64;
    let mut attempts = 3_usize;
    let mut margin = 8_u64;

    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let mut next = || {
            args.next()
                .ok_or_else(|| anyhow!("missing value for {flag}"))
        };
        match flag.as_str() {
            "--rpc" => rpc = next()?,
            "--faucet" => faucet = next()?,
            "--recipient" => recipient = next()?,
            "--min-mist" => min_mist = next()?.parse().context("--min-mist is a u64")?,
            "--attempts" => attempts = next()?.parse().context("--attempts is a usize")?,
            "--margin" => margin = next()?.parse().context("--margin is a u64")?,
            other => bail!("unknown flag {other}"),
        }
    }
    if recipient.is_empty() {
        bail!("--recipient is required");
    }

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()?;
    let balance =
        faucet::ensure_balance(&http, &faucet, &rpc, &recipient, min_mist, attempts, margin)
            .await?;
    println!("refill complete: balance {balance} MIST at {recipient}");
    Ok(())
}
