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
   │ signing │ │  Reth   │ │DA+settle│ │persist  │ │ network │ │ bridge  │
   └────┬────┘ └────┬────┘ └────┬────┘ └────┬────┘ └────┬────┘ └────┬────┘
        │           │           │           │           │           │
        └───────────┴───────────┴───────────┴───────────┴──► podseq-core (traits)
```

| Crate       | Responsibility                                         | Core trait                       |
| ----------- | ------------------------------------------------------ | -------------------------------- |
| `core`      | Interfaces and Commonware runtime bridge               | (defines them)                   |
| `engine`    | Reth Engine API client                                 | (none)                           |
| `sequencer` | Block header signing (Ed25519)                         | `BlockSigner`                    |
| `sui`       | Walrus DA + Sui settlement + bridge vault              | `DataAvailability`, `Settlement` |
| `store`     | Persistent storage + crash recovery                    | (none)                           |
| `p2p`       | Block propagation via Commonware                       | (none)                           |
| `node`      | Binary: CLI, config, runner, full node, bridge relayer | (none)                           |
