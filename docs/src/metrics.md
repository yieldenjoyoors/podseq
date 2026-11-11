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
| `podseq_sui_gas_balance_mist`        | Gauge     | Settlement key SUI balance in mistos                                  |
| `podseq_da_publish_duration_seconds` | Histogram | Walrus DA publish latency, observed per attempt (success and failure) |
| `podseq_da_publish_errors_total`     | Counter   | Total Walrus DA publish attempts that failed                          |
| `podseq_da_time_flushes_total`       | Counter   | DA batches flushed by `batch_flush_ms` age before the size threshold   |
| `podseq_settlement_duration_seconds` | Histogram | Latency of the successful Sui settlement RPC per block                |
| `podseq_settlement_height`           | Gauge     | Latest block height settled on Sui                                    |
| `podseq_bridge_deposits_total`       | Counter   | Bridge deposits minted on L2                                          |
| `podseq_bridge_withdrawals_total`    | Counter   | Bridge withdrawals released on Sui                                    |
| `podseq_deadline_status`             | Gauge     | Force-inclusion deadline verdict: 0 disabled, 1 active, 2 halted, 3 violated, 4 read error |
| `podseq_deadline_overdue_seconds`    | Gauge     | Seconds the oldest pending forced transaction is past its deadline    |

> Sequencer-only metrics (`blocks_built`, `sui_gas_balance`, `bridge_*`, `da_*`,
> `pending_blocks`) read 0 on full nodes. `podseq_block_height` and
> `podseq_settlement_height` are written by both modes: the sequencer after each
> settle, the full node per sync poll. The deadline gauges are full-node reads
> of the shared inbox and stay 0 on a sequencer.

## Alerting on sequencer failure

A full node verifies the chain from Sui and Walrus alone, so a stalled or
censoring sequencer is visible from its metrics without trusting any sequencer
endpoint. Three states are worth a page:

- Settlement stops advancing: `changes(podseq_settlement_height[10m]) == 0`.
  Blocks may still be produced over p2p, but nothing is being settled, so the
  chain cannot finalize.
- The deadline verdict leaves active. `podseq_deadline_status >= 2` means the
  oldest forced transaction blew its deadline: 2 is the correct halt, 3 means
  settlement ran past an expired deadline (the settle gate is broken). Past the
  10-minute halt margin the forced-exit path unlocks on-chain. This alert and
  the stall alert usually fire together; the deadline one carries the cause.
- The monitor goes blind: `podseq_deadline_status == 4` for a few minutes means
  Sui reads keep failing. The node keeps applying blocks but nothing is
  verifying them. `0` on a node meant to verify means `sui.inbox_id` is unset.

`podseq_deadline_overdue_seconds` gives the paging depth: it only grows past
the 10-minute margin while settlement stays stuck, so it separates a fresh
deadline miss from a chain that has been halted for hours.

Example rules:

```yaml
groups:
  - name: podseq-full-node
    rules:
      - alert: PodseqSettlementStalled
        expr: changes(podseq_settlement_height[10m]) == 0
        for: 5m
        labels:
          severity: critical
      - alert: PodseqForceInclusionHalt
        expr: podseq_deadline_status >= 2
        for: 1m
        labels:
          severity: critical
      - alert: PodseqDeadlineMonitorBlind
        expr: podseq_deadline_status == 4
        for: 5m
        labels:
          severity: warning
```

## Scraping

```yaml
scrape_configs:
  - job_name: podseq
    static_configs:
      - targets: ["localhost:9090"]
```

A pre-built Grafana dashboard config lives at `grafana/podseq-dashboard.json`.
