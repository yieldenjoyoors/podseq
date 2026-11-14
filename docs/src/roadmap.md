# Roadmap

Podseq is an EVM L2 with a single sequencer, Walrus DA, and Sui settlement.
The roadmap prioritizes concrete, high-impact features that keep the design
simple while making the chain trust-minimized and developer-friendly.

## Phase 1: Core L2

- [x] **Single sequencer**: block production via Reth Engine API.
- [x] **Walrus DA**: block data posted as blobs, verified for availability by Sui.
- [x] **Sui settlement**: `settlement.move` maps block height → Walrus blob ID.
- [x] **P2P block propagation**: Commonware broadcast so full nodes execute before
      DA confirmation.
- [x] **Persistent store**: crash recovery from local block + state storage.
- [x] **Full node sync**: syncs from DA, P2P, and settlement.
- [x] **Enshrined bridge**: `Bridge.sol` predeploy on L2 + `bridge.move` vault on
      Sui. Users bridge USDSui (and other Sui coins) to the L2. Deposits and withdrawals
      without external relayers.

## Phase 2: Trust Minimization

- [x] **Force inclusion**: a Sui-side inbox where users submit L2 transactions
      directly. The sequencer must include them within N blocks or halt. Gives every
      user an escape hatch from censorship. See [force inclusion](./force-inclusion.md).
- [x] **Exit queue**: if the sequencer halts or censors, any user can submit a
      Merkle proof of their L2 balance to the Sui bridge and exit without the
      sequencer, paying out with an owner-signed redemption. Records are ordered
      by `(height, seq)`; payouts are first-come in Sui transaction order.
      See [exit queue](./exit-queue.md).
- [x] **Sequencer failure detection**: full nodes verify the force-inclusion
      deadline from Sui state alone, halt locally on a violated gate, and export
      the verdict plus settlement lag as metrics with example alert rules. See
      [metrics](./metrics.md).

## Phase 3: Developer & User UX

- [x] **Fee capture**: all L2 fees flow to the sequencer. Gas is paid in
      bridged USDSui: users don't need a separate native token. The sequencer
      earns revenue directly from every block it produces. (Default is burned.)
- [x] **Open-source block explorer**: [blockscout](https://github.com/blockscout/blockscout)-based deployment with
      automatic contract verification via Sourcify.
- [x] **Open-source faucet**: [eth-faucet](https://github.com/chainflag/eth-faucet) preconfigured deployment.

## Phase 4: Operator Tooling

- [x] **Sequencer health dashboard**: Prometheus metrics endpoint with block
      height, DA latency, settlement duration, Sui gas balance, and bridge
      throughput. Grafana dashboard template included.
- [x] **Rate limiting on RPC**: token-bucket limiter for public RPC
      endpoints. Protects the sequencer from DDoS without affecting block
      production (Engine API has its own port). The node's proxy fronts
      Reth's public port with per-client-IP token buckets — see
      [setup](./setup.md#rate-limiting-the-public-rpc).
- [x] **Hot-swap settlement key**: rotate the `SettlerCap` / `BridgeCap`
      without downtime. The new key is pre-authorized on Sui; the node switches
      on an admin signal. Landed in the protocol and node runtime, and the
      steering ceremony is exercised on a live deployment: two full rotation
      cycles against testnet, the second staged by the cold `OpsCap` holder
      and claimed by a fresh payer — see
      [cap rotation](./cap-rotation.md#live-steering-exercise-testnet-2026-11-14).
- [x] **Automatic Sui gas top-up**: monitor the settlement key's Sui balance.
      If it drops below a threshold, transfer from a funded reserve address.
      Landed in the node's gas watcher — see
      [metrics](./metrics.md#keeping-the-settlement-key-funded).
