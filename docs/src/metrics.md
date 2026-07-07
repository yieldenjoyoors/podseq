# Metrics

Podseq exposes a Prometheus `/metrics` endpoint for monitoring sequencer health,
block production, DA/settlement performance, and bridge throughput.

## Configuration

```toml
[metrics]
enabled = true       # default: false
listen_addr = "0.0.0.0:9090"
```

When `enabled = false`, no HTTP server is started.

## Endpoints

| Path           | Description                         |
| -------------- | ----------------------------------- |
| `GET /metrics` | Prometheus text exposition format   |
| `GET /healthz` | Liveness probe (200 OK, empty body) |

## Metrics Reference

| Metric                               | Type      | Description                                                           |
| ------------------------------------ | --------- | --------------------------------------------------------------------- |
| `podseq_block_height`                | Gauge     | Current chain head height                                             |
| `podseq_blocks_built_total`          | Counter   | Total blocks produced since startup                                   |
| `podseq_pending_blocks`              | Gauge     | Blocks buffered in the finalizer channel                              |
| `podseq_da_publish_duration_seconds` | Histogram | Walrus DA publish latency, observed per attempt (success and failure) |
| `podseq_da_publish_errors_total`     | Counter   | Total Walrus DA publish attempts that failed                          |
| `podseq_settlement_duration_seconds` | Histogram | Latency of the successful Sui settlement RPC per block                |
| `podseq_bridge_deposits_total`       | Counter   | Bridge deposits minted on L2                                          |
| `podseq_bridge_withdrawals_total`    | Counter   | Bridge withdrawals released on Sui                                    |

> Sequencer-only metrics (`block_height`, `blocks_built`, `bridge_*`) read 0 on full nodes.

## Scraping

```yaml
scrape_configs:
  - job_name: podseq
    static_configs:
      - targets: ["localhost:9090"]
```

A pre-built Grafana dashboard config lives at `grafana/podseq-dashboard.json`.
