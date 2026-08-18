# Docker

Runs the full podseq stack: [Reth](https://github.com/paradigmxyz/reth) (execution)
plus podseq (sequencer/consensus), against Walrus and Sui on **testnet** or
**mainnet**. Walrus and Sui are public services, so only the two local services
are containerized.

## Files

| File                         | Purpose                                                                                       |
| ---------------------------- | --------------------------------------------------------------------------------------------- |
| `Dockerfile`                 | Multi-stage build of the `podseq` binary; ships a default config at `/etc/podseq/podseq.toml` |
| `docker/podseq.toml`         | Default config (image-baked). Override by bind-mounting a replacement.                        |
| `docker/podseq.testnet.toml` | Testnet config: public Walrus testnet + Sui testnet                                           |
| `docker/podseq.mainnet.toml` | Mainnet config: Sui mainnet + local authenticated Walrus publisher                            |
| `docker-compose.yml`         | Base stack: `init-jwt`, `reth`, `podseq`                                                      |
| `docker-compose.testnet.yml` | Testnet override: mounts `podseq.testnet.toml`, ports `8545/8551`, project `podseq-testnet`   |
| `docker-compose.mainnet.yml` | Mainnet override: mounts `podseq.mainnet.toml`, ports `8645/8651`, project `podseq-mainnet`   |

Each override sets its own project name, container names and host ports, so the
two stacks can run side by side.

## Prerequisites

- Docker with BuildKit and the Compose v2 plugin
- A funded Sui address for on-chain settlement (SUI for gas)

## Provide signing keys

Drop keys into `docker/secrets/` (gitignored). They are mounted read-only at `/secrets`:

```sh
# Signer key (suiprivkey, used for settlement txs + block signing, needs SUI for gas)
make sui-key   # runs the podseq image to generate docker/secrets/sui.key
```

The command prints the derived Sui address: fund it with testnet SUI via the
[Sui faucet](https://faucet.sui.io/) before starting the stack. An existing
`suiprivkey` works too:

```sh
echo "suiprivkey..." > docker/secrets/sui.key
chmod 600 docker/secrets/*
```

The signer key is **required** in sequencer mode: podseq uses it to sign
settlement transactions and block headers. Without it, the sequencer refuses to
start. The Engine API JWT is generated automatically and shared between
Reth and podseq.

## Configure podseq

Edit the per-network config (`docker/podseq.testnet.toml` or
`docker/podseq.mainnet.toml`) for your deployment. Common things to set:

- `[sui] settlement_package_id` / `settler_cap_id` / `registry_id` — supply the
  object IDs of an already-deployed Move contract, or leave all three unset for
  first-start auto-deploy. The deployed IDs are written back into the mounted
  config file, which is why the compose files mount it read-write.
- `[sequencer] block_time_ms`, `fee_recipient`, `genesis_hash` — production tuning.
- `[walrus] publisher_auth_token` — mainnet only; must match the
  `WALRUS_PUBLISHER_AUTH_TOKEN` env var passed to the publisher service.

For deeper customization, bind-mount your own TOML:

```yaml
volumes:
  - ./my-podseq.toml:/etc/podseq/podseq.toml:ro
```

## Run

```sh
# Testnet
docker compose -f docker-compose.yml -f docker-compose.testnet.yml up -d --build

# Mainnet (requires WALRUS_PUBLISHER_AUTH_TOKEN in env)
WALRUS_PUBLISHER_AUTH_TOKEN=... \
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
supply the object IDs in the per-network config, or let podseq auto-deploy on
first start.

For a **first-start auto-deploy**, podseq reads
`move/build/podseq_settlement/bytecode.mv`. Build it locally and bind-mount it,
or bake it into the image:

```sh
sui move build --path move
# then mount the build output into the container at /app/move/build
```

See `docs/src/contract.md`.

## Notes

- Reth runs with `--chain=dev` so the stack starts without a custom genesis. For
  a production L2, replace the `reth` service `command` with your own chain spec
  (podseq drives Reth purely over the Engine API).
- Pin `ghcr.io/paradigmxyz/reth:latest` to a specific tag for production.
- Verify the Walrus mainnet endpoints against the Walrus docs before mainnet use.
