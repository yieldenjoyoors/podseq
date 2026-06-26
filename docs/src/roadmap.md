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
- [ ] **Enshrined bridge**: `Bridge.sol` predeploy on L2 + `bridge.move` vault on
      Sui. Users bridge USDSui (and other Sui coins) to the L2. Deposits and withdrawals
      without external relayers.

## Phase 2: Trust Minimization

- [ ] **Force inclusion**: a Sui-side inbox where users submit L2 transactions
      directly. The sequencer must include them within N blocks or halt. Gives every
      user an escape hatch from censorship.
- [ ] **Exit queue**: withdrawals are ordered by L2 block height so the
      sequencer cannot reorder exits to front-run users. If the sequencer stops
      processing the queue for > T seconds, any user can submit a Merkle proof of
      their L2 balance to the Sui bridge and exit without the sequencer.
- [ ] **Sequencer failure detection**: full nodes monitor settlement height
      and exit queue liveness. If the sequencer stalls, an alert fires and the
      forced exit path unlocks.

## Phase 3: Developer & User UX

- [x] **Fee capture**: all L2 fees flow to the sequencer. Gas is paid in
      bridged USDSui: users don't need a separate native token. The sequencer
      earns revenue directly from every block it produces. (Default is burned.)
- [ ] **Open-source block explorer**: [blockscout](https://github.com/blockscout/blockscout)-based deployment with
      automatic contract verification via Sourcify.
- [ ] **Open-source faucet**: [eth-faucet](https://github.com/chainflag/eth-faucet) preconfigured deployment.

## Phase 4: Operator Tooling

- [ ] **Sequencer health dashboard**: Prometheus metrics endpoint with block
      height, DA latency, Sui gas balance, pending deposit queue depth, and p2p
      peer count. Grafana dashboard template included.
- [ ] **Rate limiting on RPC**: token-bucket limiter for public RPC
      endpoints. Protects the sequencer from DDoS without affecting block
      production (Engine API has its own port).
- [ ] **Hot-swap settlement key**: rotate the `SettlerCap` / `BridgeCap`
      without downtime. The new key is pre-authorized on Sui; the node switches
      on an admin signal.
- [ ] **Automatic Sui gas top-up**: monitor the settlement key's Sui balance.
      If it drops below a threshold, transfer from a funded reserve address.
