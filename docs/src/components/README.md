# Components

Podseq is split into crates, each owning one concern. They communicate only through the
traits defined in [`podseq-core`](./core.md).

```text
                   ┌──────────────────────────┐
                   │        node (bin)        │
                   │  wires everything together │
                   │  (+ store, + signing)    │
                   └────────────┬─────────────┘
        ┌───────────┬──────────┼──────────┐
        ▼           ▼          ▼          ▼
   ┌─────────┐ ┌─────────┐ ┌─────────┐ ┌─────────┐
   │  core   │ │ engine  │ │   sui   │ │   p2p   │
   │ traits  │ │  Reth   │ │DA+settle│ │ network │
   │ +signer │ │         │ │+ bridge │ │         │
   └─────────┘ └─────────┘ └─────────┘ └─────────┘
```

| Crate    | Responsibility                                                             | Core trait                       |
| -------- | -------------------------------------------------------------------------- | -------------------------------- |
| `core`   | Interfaces, types, `Ed25519BlockSigner`, Commonware runtime bridge         | (defines them)                   |
| `engine` | Reth Engine API client                                                     | (none)                           |
| `sui`    | Walrus DA + Sui settlement + bridge vault                                  | `DataAvailability`, `Settlement` |
| `p2p`    | Block propagation via Commonware                                           | (none)                           |
| `node`   | Binary: CLI, config, runner, full node, bridge relayer, persistent storage | (none)                           |
