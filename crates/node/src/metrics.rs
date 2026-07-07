//! Prometheus metrics for the podseq sequencer.
//!
//! Exposes a `/metrics` HTTP endpoint in the Prometheus text exposition format
//! when `[metrics] enabled = true` is set in the config. The endpoint is served
//! on a background tokio task and does not block the sequencer loop.
//!
//! Metric naming follows the Prometheus convention:
//! - Counters end with `_total`
//! - Histograms end with `_duration_seconds`
//! - Gauges have no suffix

use std::net::SocketAddr;
use std::sync::Arc;

use prometheus_client::encoding::text::encode;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::{exponential_buckets, Histogram};
use prometheus_client::registry::Registry;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::{error, info};

/// All metric instruments used by the sequencer.
///
/// Constructed once via [`PodseqMetrics::new`] and shared as `Arc<PodseqMetrics>`
/// across the runner, bridge, and HTTP server.
#[derive(Debug)]
pub struct PodseqMetrics {
    pub registry: Registry,
    /// Current chain head height.
    pub block_height: Gauge,
    /// Total blocks produced since startup.
    pub blocks_built: Counter,
    /// Walrus DA publish latency per attempt (success and failure).
    pub da_publish_duration: Histogram,
    /// Total Walrus DA publish attempts that failed.
    pub da_publish_errors_total: Counter,
    /// Latency of the successful Sui settlement RPC for each block.
    pub settlement_duration: Histogram,
    /// Blocks buffered in the finalizer channel.
    pub pending_blocks: Gauge,
    /// Settlement key's SUI balance in mistos.
    pub sui_gas_balance: Gauge,
    /// Bridge deposits minted on L2.
    pub bridge_deposits_total: Counter,
    /// Bridge withdrawals released on Sui.
    pub bridge_withdrawals_total: Counter,
}

impl PodseqMetrics {
    /// Creates a new metrics instance with all instruments registered.
    pub fn new() -> Self {
        let mut registry = Registry::default();

        let block_height = Gauge::default();
        registry.register(
            "podseq_block_height",
            "Current chain head height",
            block_height.clone(),
        );

        let blocks_built = Counter::default();
        registry.register(
            "podseq_blocks_built_total",
            "Total blocks produced since startup",
            blocks_built.clone(),
        );

        let da_publish_duration = Histogram::new(exponential_buckets(0.01, 2.0, 13));
        registry.register(
            "podseq_da_publish_duration_seconds",
            "Walrus DA publish latency per attempt (success and failure)",
            da_publish_duration.clone(),
        );

        let da_publish_errors_total = Counter::default();
        registry.register(
            "podseq_da_publish_errors_total",
            "Total Walrus DA publish attempts that failed",
            da_publish_errors_total.clone(),
        );

        let settlement_duration = Histogram::new(exponential_buckets(0.01, 2.0, 13));
        registry.register(
            "podseq_settlement_duration_seconds",
            "Latency of the successful Sui settlement RPC for each block",
            settlement_duration.clone(),
        );

        let pending_blocks = Gauge::default();
        registry.register(
            "podseq_pending_blocks",
            "Blocks buffered in the finalizer channel",
            pending_blocks.clone(),
        );

        let sui_gas_balance = Gauge::default();
        registry.register(
            "podseq_sui_gas_balance_mist",
            "Settlement key SUI balance in mistos",
            sui_gas_balance.clone(),
        );

        let bridge_deposits_total = Counter::default();
        registry.register(
            "podseq_bridge_deposits_total",
            "Bridge deposits minted on L2",
            bridge_deposits_total.clone(),
        );

        let bridge_withdrawals_total = Counter::default();
        registry.register(
            "podseq_bridge_withdrawals_total",
            "Bridge withdrawals released on Sui",
            bridge_withdrawals_total.clone(),
        );

        Self {
            registry,
            block_height,
            blocks_built,
            da_publish_duration,
            da_publish_errors_total,
            settlement_duration,
            pending_blocks,
            sui_gas_balance,
            bridge_deposits_total,
            bridge_withdrawals_total,
        }
    }
}

impl Default for PodseqMetrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Serves the Prometheus text exposition format on a TCP listener.
///
/// Binds to `addr` and responds to every HTTP request with the full metrics
/// payload. A GET to `/healthz` returns 200 OK with an empty body for
/// liveness probes. The task runs until the provided [`Notify`] is signalled.
pub async fn serve(
    metrics: Arc<PodseqMetrics>,
    addr: SocketAddr,
    shutdown: Arc<tokio::sync::Notify>,
) {
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            error!(addr = %addr, error = %e, "failed to bind metrics endpoint");
            return;
        }
    };
    info!(addr = %addr, "metrics endpoint listening");

    loop {
        let (stream, _peer) = match tokio::select! {
            result = listener.accept() => result,
            _ = shutdown.notified() => break,
        } {
            Ok(v) => v,
            Err(e) => {
                error!(error = %e, "metrics: accept failed");
                continue;
            }
        };

        let metrics = Arc::clone(&metrics);
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, &metrics).await {
                error!(error = %e, "metrics: connection handler failed");
            }
        });
    }

    info!("metrics endpoint stopped");
}

async fn handle_connection(
    mut stream: tokio::net::TcpStream,
    metrics: &PodseqMetrics,
) -> anyhow::Result<()> {
    let (reader, mut writer) = stream.split();
    let mut reader = BufReader::new(reader);
    let mut request_line = String::new();
    reader.read_line(&mut request_line).await?;

    let path = request_line.split_whitespace().nth(1).unwrap_or("/");

    let (status, body) = if path == "/healthz" {
        ("200 OK", Vec::new())
    } else {
        let mut buffer = String::new();
        encode(&mut buffer, &metrics.registry)?;
        ("200 OK", buffer.into_bytes())
    };

    let content_type = if path == "/healthz" {
        "text/plain"
    } else {
        "text/plain; version=0.0.4; charset=utf-8"
    };

    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );

    writer.write_all(response.as_bytes()).await?;
    writer.write_all(&body).await?;
    writer.flush().await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_metrics_registers_all_instruments() {
        let m = PodseqMetrics::new();
        // Verify all instruments are usable by recording a value.
        m.block_height.set(42);
        m.blocks_built.inc();
        m.da_publish_duration.observe(0.5);
        m.da_publish_errors_total.inc();
        m.settlement_duration.observe(1.0);
        m.pending_blocks.set(3);
        m.sui_gas_balance.set(1_000_000_000);
        m.bridge_deposits_total.inc();
        m.bridge_withdrawals_total.inc();
    }

    #[test]
    fn encode_produces_valid_prometheus_text() {
        let m = PodseqMetrics::new();
        m.block_height.set(100);
        m.blocks_built.inc();
        m.blocks_built.inc();

        let mut buffer = String::new();
        encode(&mut buffer, &m.registry).unwrap();

        assert!(buffer.contains("podseq_block_height"));
        assert!(buffer.contains("100"));
        assert!(buffer.contains("podseq_blocks_built_total"));
        assert!(buffer.contains("2"));
    }

    #[test]
    fn default_creates_usable_instance() {
        let m = PodseqMetrics::default();
        m.block_height.set(1);
    }
}
