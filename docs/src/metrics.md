# Metrics

Podseq exposes Prometheus metrics for monitoring sequencer health, block production,
and DA/settlement performance.

## Configuration

Add a `[metrics]` section to your config:

```toml
[metrics]
enabled = true
listen_addr = "0.0.0.0:9090"
```

When `enabled = false` (the default), no HTTP server is started and no metrics are
collected.

## Endpoints

| Path | Description |
|------|-------------|
| `GET /metrics` | Prometheus text exposition format |
| `GET /healthz` | Liveness probe (returns 200 OK) |

## Metrics Reference

### Block Production

| Metric | Type | Description |
|--------|------|-------------|
| `podseq_block_height` | Gauge | Current chain head height |
| `podseq_blocks_built_total` | Counter | Total blocks produced since startup |
| `podseq_pending_blocks` | Gauge | Blocks buffered in the finalizer channel |

### Data Availability

| Metric | Type | Description |
|--------|------|-------------|
| `podseq_da_publish_duration_seconds` | Histogram | Walrus DA publish latency |

### Settlement

| Metric | Type | Description |
|--------|------|-------------|
| `podseq_settlement_duration_seconds` | Histogram | Total time to settle a block on Sui, including retries until success |

### Bridge

| Metric | Type | Description |
|--------|------|-------------|
| `podseq_bridge_deposits_total` | Counter | Bridge deposits minted on L2 |
| `podseq_bridge_withdrawals_total` | Counter | Bridge withdrawals released on Sui |

## Grafana Dashboard

A pre-built Grafana dashboard is available at `grafana/podseq-dashboard.json`.

To import:

1. Open Grafana → Dashboards → Import
2. Upload `grafana/podseq-dashboard.json`
3. Select your Prometheus data source
4. Click Import

## Example Output

```
$ curl -s localhost:9090/metrics
# HELP podseq_block_height Current chain head height
# TYPE podseq_block_height gauge
podseq_block_height 1247
# HELP podseq_blocks_built_total Total blocks produced since startup
# TYPE podseq_blocks_built_total counter
podseq_blocks_built_total 1247
# HELP podseq_pending_blocks Blocks buffered in the finalizer channel
# TYPE podseq_pending_blocks gauge
podseq_pending_blocks 0
# HELP podseq_da_publish_duration_seconds Walrus DA publish latency
# TYPE podseq_da_publish_duration_seconds histogram
podseq_da_publish_duration_seconds_bucket{le="0.01"} 0
...
# HELP podseq_settlement_duration_seconds Sui settlement commit latency
# TYPE podseq_settlement_duration_seconds histogram
podseq_settlement_duration_seconds_bucket{le="0.01"} 0
...
# HELP podseq_bridge_deposits_total Bridge deposits minted on L2
# TYPE podseq_bridge_deposits_total counter
podseq_bridge_deposits_total 0
# HELP podseq_bridge_withdrawals_total Bridge withdrawals released on Sui
# TYPE podseq_bridge_withdrawals_total counter
podseq_bridge_withdrawals_total 0
```

## Prometheus Scrape Config

```yaml
scrape_configs:
  - job_name: podseq
    static_configs:
      - targets: ["localhost:9090"]
    scrape_interval: 15s
```
