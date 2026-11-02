//! Registry-published committee peers: read model and publish PTB.
//!
//! The settlement Registry carries a `PeerBook` dynamic field mapping each
//! committee member's ed25519 p2p identity to its dialable address. The
//! sequencer publishes itself at startup; full nodes read the book over the
//! same transport as the inbox and admit exactly these peers, so peering
//! needs no sequencer-side bootstrap configuration.

use sui_sdk_types::Address;
use sui_sdk_types::{Identifier, StructTag, TypeTag};
use sui_transaction_builder::{Function, ObjectInput, TransactionBuilder};

use crate::deadline::SUI_CLOCK_ID;
use crate::inbox::{InboxError, InboxTransport};
use crate::settlement::SettlementError;

/// BCS encoding of the `settlement::PeerKey` unit struct: no fields, no bytes.
pub const PEER_KEY_BCS: &[u8] = &[];

/// Returns the `settlement::PeerKey` type tag under `package`.
#[must_use]
fn peer_key_type_tag(package: Address) -> TypeTag {
    // StructTag's fields are private; the string form parses into the same
    // canonical tag the Move module declares.
    let tag: StructTag = format!("{package}::settlement::PeerKey")
        .parse()
        .expect("static type tag");
    TypeTag::Struct(Box::new(tag))
}

/// Returns the dynamic-field object id holding the `PeerBook` on `parent`.
///
/// `parent` is the shared Registry object id; `package` is the deployed Move
/// package id. Mirrors `settlement::PeerKey`.
#[must_use]
pub fn peer_book_field_id(parent: Address, package: Address) -> Address {
    parent.derive_dynamic_child_id(&peer_key_type_tag(package), PEER_KEY_BCS)
}

/// One published committee peer, mirrored from a `PeerBook` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedPeer {
    /// ed25519 p2p identity (32 bytes, hex-encoded in config files).
    pub pubkey: [u8; 32],
    /// Dialable host:port the peer advertises.
    pub addr: String,
}

/// A versioned snapshot of the peer book.
///
/// `version` is the Move-side mutation counter and doubles as the discovery
/// peer-set index: every node registering version N must register identical
/// content, or the discovery gossip kills the connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerBookSnapshot {
    pub version: u64,
    pub peers: Vec<PublishedPeer>,
}

/// Decodes the versioned peer book from raw `Field<PeerKey, PeerBook>` BCS.
///
/// # Errors
///
/// [`SettlementError::Parse`] when the BCS does not decode or an entry
/// identity is not 32 bytes.
pub fn parse_peer_book(field_bcs: &[u8]) -> Result<PeerBookSnapshot, SettlementError> {
    #[derive(serde::Deserialize)]
    struct EntryBcs {
        key: Vec<u8>,
        value: String,
    }

    #[derive(serde::Deserialize)]
    struct BookBcs {
        version: u64,
        peers: Vec<EntryBcs>,
    }

    // `name` (the zero-sized PeerKey) sits between `id` and `value` on the
    // wire but contributes no bytes, so a two-field mirror reads it fine.
    #[derive(serde::Deserialize)]
    #[allow(dead_code)]
    struct FieldBcs {
        id: Address,
        value: BookBcs,
    }

    let field: FieldBcs = bcs::from_bytes(field_bcs)
        .map_err(|e| SettlementError::Parse(format!("peer book BCS: {e}")))?;
    let peers = field
        .value
        .peers
        .into_iter()
        .map(|e| {
            let key_len = e.key.len();
            let key: [u8; 32] = e.key.try_into().map_err(|_| {
                SettlementError::Parse(format!("peer identity must be 32 bytes, got {key_len}"))
            })?;
            Ok::<_, SettlementError>(PublishedPeer {
                pubkey: key,
                addr: e.value,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(PeerBookSnapshot {
        version: field.value.version,
        peers,
    })
}

/// Reads the peer book over the mockable transport.
#[derive(Debug, Clone)]
pub struct PeerBookReader<T> {
    transport: T,
    registry: Address,
    package: Address,
}

impl<T: InboxTransport> PeerBookReader<T> {
    /// Creates a reader for the Registry at `registry` under `package`.
    pub fn new(transport: T, registry: Address, package: Address) -> Self {
        Self {
            transport,
            registry,
            package,
        }
    }

    /// The versioned committee book from chain state.
    ///
    /// `Ok` with version 0 and no peers when no book was published yet (a
    /// fresh deployment); admission then keeps configured bootstrappers.
    /// Transport and parse errors propagate: an unreadable book must never
    /// be read as "no peers", or a Sui partition would silently shrink the
    /// committee.
    ///
    /// # Errors
    ///
    /// [`InboxError::Rpc`] on transport failure, [`InboxError::NotFound`]
    /// mapped to an empty version-0 snapshot, [`InboxError::Parse`] when
    /// the stored BCS does not decode.
    pub async fn snapshot(&self) -> Result<PeerBookSnapshot, InboxError> {
        let id = peer_book_field_id(self.registry, self.package);
        match self.transport.object_contents(&id).await {
            Err(InboxError::NotFound(_)) => Ok(PeerBookSnapshot {
                version: 0,
                peers: Vec::new(),
            }),
            Err(e) => Err(e),
            Ok(bytes) => parse_peer_book(&bytes).map_err(|e| InboxError::Parse(e.to_string())),
        }
    }
}

/// Objects one publish PTB touches.
#[derive(Debug, Clone)]
pub struct PeerTargets {
    /// Deployed Move package id.
    pub package: Address,
    /// Shared settlement Registry object id.
    pub registry: Address,
}

/// Publishes `pubkey -> addr` in one PTB signed by the settlement authority
/// key. Idempotent: re-publishing upserts the dial address on-chain.
///
/// # Errors
///
/// [`SettlementError`] on build, RPC, or execution failure; an on-chain
/// abort (e.g. `E_NOT_AUTHORIZED` after a rotation, outside grace)
/// surfaces as an execution error.
pub async fn submit_publish_peer(
    rpc_url: &str,
    key: &sui_crypto::ed25519::Ed25519PrivateKey,
    targets: &PeerTargets,
    pubkey: [u8; 32],
    addr: &str,
) -> Result<(), SettlementError> {
    let sender = key.public_key().derive_address();
    let mut builder = TransactionBuilder::new();
    builder.set_sender(sender);
    let registry = builder.object(ObjectInput::new(targets.registry));
    let clock = builder.object(ObjectInput::new(SUI_CLOCK_ID));
    let pubkey_arg = builder.pure(&pubkey.to_vec());
    let addr_arg = builder.pure(&addr.as_bytes().to_vec());
    builder.move_call(
        Function::new(
            targets.package,
            Identifier::new("settlement").map_err(|e| SettlementError::Parse(e.to_string()))?,
            Identifier::new("publish_peer").map_err(|e| SettlementError::Parse(e.to_string()))?,
        ),
        vec![registry, clock, pubkey_arg, addr_arg],
    );

    let mut rpc = sui_rpc::Client::new(rpc_url).map_err(|e| SettlementError::Rpc(e.to_string()))?;
    let tx = builder
        .build(&mut rpc)
        .await
        .map_err(|e| SettlementError::Build(format!("publish peer: {e}")))?;
    let mut rpc2 =
        sui_rpc::Client::new(rpc_url).map_err(|e| SettlementError::Rpc(e.to_string()))?;
    crate::settlement::sign_and_execute(
        &mut rpc2,
        key,
        tx,
        "publish peer",
        crate::settlement::DEPLOY_TX_WAIT,
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// In-memory transport mirroring the two `get_object` shapes.
    #[derive(Default)]
    struct MockTransport {
        objects: BTreeMap<Address, Vec<u8>>,
        fail_objects: bool,
    }

    impl InboxTransport for MockTransport {
        fn object_contents<'a>(
            &'a self,
            id: &'a Address,
        ) -> crate::inbox::InboxFuture<'a, Result<Vec<u8>, InboxError>> {
            Box::pin(async move {
                if self.fail_objects {
                    return Err(InboxError::Rpc("transport down".into()));
                }
                self.objects
                    .get(id)
                    .cloned()
                    .ok_or_else(|| InboxError::NotFound(format!("no object {id}")))
            })
        }

        fn field_contents<'a>(
            &'a self,
            _table: &'a Address,
            _key: u64,
        ) -> crate::inbox::InboxFuture<'a, Result<Option<Vec<u8>>, InboxError>> {
            Box::pin(async move { Ok(None) })
        }
    }

    /// Serde mirrors of the Move-side `PeerBook` layout, used only to build
    /// test fixtures byte-for-byte the way `settlement.move` stores them.
    mod mirror {
        use super::*;

        #[derive(serde::Serialize)]
        pub struct Entry {
            pub key: Vec<u8>,
            pub value: String,
        }

        #[derive(serde::Serialize)]
        pub struct Book {
            pub version: u64,
            pub peers: Vec<Entry>,
        }

        #[derive(serde::Serialize)]
        pub struct Field {
            pub id: Address,
            pub value: Book,
        }
    }

    use mirror::{Book, Entry, Field};

    fn registry() -> Address {
        Address::new([0x33; 32])
    }

    fn package() -> Address {
        Address::new([0x44; 32])
    }

    fn field_bytes(version: u64, peers: Vec<([u8; 32], &str)>) -> Vec<u8> {
        let field = Field {
            id: Address::new([0; 32]),
            value: Book {
                version,
                peers: peers
                    .into_iter()
                    .map(|(k, v)| Entry {
                        key: k.to_vec(),
                        value: v.to_string(),
                    })
                    .collect(),
            },
        };
        bcs::to_bytes(&field).unwrap()
    }

    #[test]
    fn parse_peer_book_roundtrips_version_and_entries_in_order() {
        let bytes = field_bytes(7, vec![([0xaa; 32], "seq:7000"), ([0xbb; 32], "seq2:7000")]);

        let snapshot = parse_peer_book(&bytes).unwrap();

        assert_eq!(snapshot.version, 7);
        assert_eq!(snapshot.peers.len(), 2);
        assert_eq!(snapshot.peers[0].pubkey, [0xaa; 32]);
        assert_eq!(snapshot.peers[0].addr, "seq:7000");
        assert_eq!(snapshot.peers[1].pubkey, [0xbb; 32]);
        assert_eq!(snapshot.peers[1].addr, "seq2:7000");
    }

    #[test]
    fn parse_peer_book_accepts_an_empty_book() {
        let bytes = field_bytes(3, vec![]);

        let snapshot = parse_peer_book(&bytes).unwrap();

        assert_eq!(snapshot.version, 3);
        assert_eq!(snapshot.peers, Vec::<PublishedPeer>::new());
    }

    #[test]
    fn parse_peer_book_rejects_truncated_bcs() {
        let full = field_bytes(1, vec![([0xaa; 32], "seq:7000")]);

        assert!(parse_peer_book(&full[..full.len() - 1]).is_err());
        assert!(parse_peer_book(&[0u8; 8]).is_err());
    }

    #[test]
    fn peer_book_field_id_is_deterministic_and_distinct() {
        let other_registry = Address::new([0x34; 32]);
        let other_package = Address::new([0x45; 32]);

        assert_eq!(
            peer_book_field_id(registry(), package()),
            peer_book_field_id(registry(), package())
        );
        assert_ne!(
            peer_book_field_id(registry(), package()),
            peer_book_field_id(other_registry, package())
        );
        assert_ne!(
            peer_book_field_id(registry(), package()),
            peer_book_field_id(registry(), other_package)
        );
    }

    #[tokio::test]
    async fn reader_lists_published_peers() {
        let mut transport = MockTransport::default();
        transport
            .objects
            .insert(peer_book_field_id(registry(), package()), {
                field_bytes(4, vec![([0xaa; 32], "seq:7000")])
            });
        let reader = PeerBookReader::new(transport, registry(), package());

        let snapshot = reader.snapshot().await.unwrap();

        assert_eq!(snapshot.version, 4);
        assert_eq!(
            snapshot.peers,
            vec![PublishedPeer {
                pubkey: [0xaa; 32],
                addr: "seq:7000".into(),
            }]
        );
    }

    #[tokio::test]
    async fn reader_treats_a_missing_book_as_version_zero_and_empty() {
        let transport = MockTransport::default();
        let reader = PeerBookReader::new(transport, registry(), package());

        let snapshot = reader.snapshot().await.unwrap();

        assert_eq!(snapshot.version, 0);
        assert_eq!(snapshot.peers, Vec::<PublishedPeer>::new());
    }

    #[tokio::test]
    async fn reader_propagates_transport_errors() {
        let transport = MockTransport {
            fail_objects: true,
            ..MockTransport::default()
        };
        let reader = PeerBookReader::new(transport, registry(), package());

        assert!(matches!(reader.snapshot().await, Err(InboxError::Rpc(_))));
    }

    #[tokio::test]
    async fn reader_surfaces_parse_errors() {
        let mut transport = MockTransport::default();
        transport
            .objects
            .insert(peer_book_field_id(registry(), package()), vec![0u8; 9]);
        let reader = PeerBookReader::new(transport, registry(), package());

        assert!(matches!(reader.snapshot().await, Err(InboxError::Parse(_))));
    }
}
