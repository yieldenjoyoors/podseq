//! Returns leftover bench SUI to the faucet's return address.
//!
//! The bench addresses are balance-model (no coin object listings), so the
//! return is a `SplitCoins(Gas)` PTB, one per key, with the amount computed
//! from the live balance minus a gas reserve. Dev-only helper, same shape as
//! the rotation bench's `fund_staged`.
//!
//! Usage:
//!
//! ```text
//! cargo run -p podseq-sui --example return_bench_sui -- \
//!     <rpc_url> <key_path> <recipient> <reserve_mist>
//! ```

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use sui_crypto::ed25519::Ed25519PrivateKey;
use sui_crypto::SuiSigner;
use sui_rpc::proto::sui::rpc::v2::ExecuteTransactionRequest;
use sui_rpc::Client as RpcClient;
use sui_sdk_types::Address;
use sui_transaction_builder::TransactionBuilder;

/// Per-transaction wait for checkpoint inclusion.
const TX_WAIT: Duration = Duration::from_secs(60);

/// Amount kept behind for the return transaction's own gas.
const GAS_RESERVE_MIST: u64 = 500_000_000;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let rpc_url = args
        .next()
        .context("usage: <rpc_url> <key_path> <recipient> [reserve_mist]")?;
    let key_path = args.next().context("missing key path")?;
    let recipient: Address = args
        .next()
        .context("missing recipient")?
        .parse()
        .context("parsing recipient address")?;
    let reserve = args
        .next()
        .map(|v| v.parse::<u64>())
        .transpose()
        .context("parsing reserve")?
        .unwrap_or(GAS_RESERVE_MIST);

    let key = load_key(Path::new(&key_path))?;
    let sender = key.public_key().derive_address();

    let mut rpc = RpcClient::new(&rpc_url)
        .map_err(|e| anyhow::anyhow!("rpc client: {e}"))
        .context("building rpc client")?;
    let balance = podseq_sui::settlement::sui_balance(&rpc_url, &sender.to_string())
        .await
        .context("reading balance")?;
    let amount = balance
        .checked_sub(reserve)
        .context("balance below the gas reserve")?;
    println!("{sender} balance {balance} → returning {amount}, reserving {reserve}");

    let mut builder = TransactionBuilder::new();
    builder.set_sender(sender);
    let gas = builder.gas();
    let amount_arg = builder.pure(&amount);
    let splits = builder.split_coins(gas, vec![amount_arg]);
    let to = builder.pure(&recipient);
    builder.transfer_objects(splits, to);

    let tx = builder.build(&mut rpc).await.context("building PTB")?;
    let digest = tx.digest().to_string();
    let signature = key.sign_transaction(&tx).context("signing PTB")?;
    rpc.execute_transaction_and_wait_for_checkpoint(
        ExecuteTransactionRequest::new(tx.into()).with_signatures(vec![signature.into()]),
        TX_WAIT,
    )
    .await
    .context("executing PTB")?;
    println!("{sender} returned {amount} MIST to {recipient} in {digest}");

    Ok(())
}

fn load_key(path: &Path) -> Result<Ed25519PrivateKey> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    podseq_sui::parse_signer_key(raw.trim()).map_err(|e| anyhow::anyhow!("parsing key: {e}"))
}
