# Docker

Runs the full podseq stack: [Reth](https://github.com/paradigmxyz/reth) (execution)
plus podseq (sequencer/consensus), against Walrus and Sui on **testnet** or
**mainnet**. Walrus and Sui are public services, so only the two local services
are containerized.

## Files

| File                         | Purpose                                                        |
| ---------------------------- | -------------------------------------------------------------- |
| `Dockerfile`                 | Multi-stage build of the `podseq` binary                       |
| `docker/entrypoint.sh`       | Renders the podseq TOML config from env vars, then runs it     |
| `docker-compose.yml`         | Base stack: `init-jwt`, `reth`, `podseq`                       |
| `docker-compose.testnet.yml` | Testnet endpoints, ports `8545/8551`, project `podseq-testnet` |
| `docker-compose.mainnet.yml` | Mainnet endpoints, ports `8645/8651`, project `podseq-mainnet` |

Each override sets its own project name, container names and host ports, so the
two stacks can run side by side.

## Prerequisites

- Docker with BuildKit and the Compose v2 plugin
- A funded Sui address for on-chain settlement (SUI for gas)

## Provide signing keys

Drop keys into `docker/secrets/` (gitignored). They are mounted read-only at `/secrets`:

```sh
# Signer key (suiprivkey, used for settlement txs + block signing, needs SUI for gas)
echo "suiprivkey..." > docker/secrets/sui.key
chmod 600 docker/secrets/*
```

The signer key is **required** in sequencer mode: podseq uses it to sign
settlement transactions and block headers. Without it, the sequencer refuses to
start. The Engine API JWT is generated automatically and shared between
Reth and podseq.

## Run

```sh
# Testnet
docker compose -f docker-compose.yml -f docker-compose.testnet.yml up -d --build

# Mainnet
docker compose -f docker-compose.yml -f docker-compose.mainnet.yml up -d --build

# Logs
docker compose -f docker-compose.yml -f docker-compose.testnet.yml logs -f podseq

# Stop / remove
docker compose -f docker-compose.yml -f docker-compose.testnet.yml down
```

### Ports

| Service | Testnet          | Mainnet          |
| ------- | ---------------- | ---------------- |
| RPC     | `localhost:8545` | `localhost:8645` |
| Engine  | `localhost:8551` | `localhost:8651` |

## Settlement

Settlement is **required** for the sequencer: every produced block is committed
to the Sui Registry, which full nodes read to verify data availability. Either
supply the object IDs of an already-deployed contract, or let podseq auto-deploy
on first start.

```yaml
environment:
  SUI_SETTLEMENT_PACKAGE_ID: 0x...
  SUI_SETTLER_CAP_ID: 0x...
  SUI_REGISTRY_ID: 0x...
```

With `docker/secrets/sui.key` present and those three IDs set, podseq signs and submits
settlement transactions in-process. See `docs/src/contract.md`.

For a **first-start auto-deploy**, podseq reads
`move/build/podseq_settlement/bytecode.mv`. Build it locally and bind-mount it,
or bake it into the image:

```sh
sui move build --path move
# then mount the build output into the container at /app/move/build
```

## Tuning

Common overrides via the `podseq` service `environment`:

| Env var         | Default     | Effect                                |
| --------------- | ----------- | ------------------------------------- |
| `BLOCK_TIME_MS` | `2000`      | Block production interval             |
| `FEE_RECIPIENT` | `0x…0`      | Fee recipient address                 |
| `GENESIS_HASH`  | unset       | Initial head hash (else queries Reth) |
| `PODSEQ_MODE`   | `sequencer` | `sequencer` or `full`                 |

## Notes

- Reth runs with `--chain=dev` so the stack starts without a custom genesis. For
  a production L2, replace the `reth` service `command` with your own chain spec
  (podseq drives Reth purely over the Engine API).
- Pin `ghcr.io/paradigmxyz/reth:latest` to a specific tag for production.
- Verify the Walrus mainnet endpoints against the Walrus docs before mainnet use.
