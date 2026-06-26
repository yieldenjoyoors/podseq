# Components

Podseq is split into crates, each owning one concern. They communicate only through the
traits defined in [`podseq-core`](./core.md).

```text
                   ┌──────────────────────────┐
                   │        node (bin)        │
                   │  wires everything together │
                   └────────────┬─────────────┘
        ┌───────────┬──────────┼──────────┬────────────┬──────────┐
        ▼           ▼          ▼          ▼            ▼          ▼
   ┌─────────┐ ┌─────────┐ ┌─────────┐ ┌─────────┐ ┌─────────┐ ┌─────────┐
   │sequencer│ │ engine  │ │   sui   │ │  store  │ │   p2p   │ │ runtime │
   │ ordering│ │  Reth   │ │DA+settle│ │persist  │ │ network │ │ bridge  │
   └────┬────┘ └────┬────┘ └────┬────┘ └────┬────┘ └────┬────┘ └────┬────┘
        │           │           │           │           │           │
        └───────────┴───────────┴───────────┴───────────┴──► podseq-core (traits)
```

| Crate       | Responsibility                             | Core trait                       |
| ----------- | ------------------------------------------ | -------------------------------- |
| `core`      | Interfaces and Commonware runtime bridge   | (defines them)                   |
| `engine`    | Reth Engine API client                     | `Executor`                       |
| `sequencer` | Transaction ordering and block signing     | `Sequencer`, `BlockSigner`       |
| `sui`       | Walrus DA + Sui settlement (shared wallet) | `DataAvailability`, `Settlement` |
| `store`     | Persistent storage + crash recovery        | (none)                           |
| `p2p`       | Block propagation via Commonware           | (none)                           |
| `node`      | Binary: CLI, config, runner, full node     | (none)                           |
