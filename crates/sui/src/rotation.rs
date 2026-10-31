//! Cap-rotation read model and derived-role state machine.
//!
//! Mirrors `move/sources/rotation.move`: the authority record is a dynamic
//! field (`rotation::StateKey`) on the settlement Registry and on the bridge
//! Vault, and the node derives its role from chain state alone — the same
//! read-from-chain design as the deadline monitor. On every poll (and
//! therefore on every boot) the watcher reads both `RotationState` records
//! plus the Sui `Clock` over the same mocked transport the inbox reader uses,
//! folds them into a [`Plan`], and executes at most one step:
//!
//! - idle states keep the configured signer key in charge,
//! - a claimable pre-authorization is claimed with the staged key in one PTB
//!   for both objects,
//! - a confirmed claim adopts the staged key in place (block production and
//!   the relayer keep running through the swap), then retires the previous
//!   authority after a quiet interval,
//! - an authority that matches neither configured key is a misconfiguration:
//!   the node refuses to claim or adopt and logs loudly; on-chain the settle
//!   gate rejects the stale key, which degrades to the visible K2/K3 halt.
//!
//! Every step is an idempotent on-chain transition, so a crash at any point
//! replays to the same result. Design record: docs/src/cap-rotation.md.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use sui_sdk_types::{Address, Identifier, StructTag, TypeTag};
use sui_transaction_builder::{Function, ObjectInput, TransactionBuilder};

use crate::deadline::{parse_clock, SUI_CLOCK_ID};
use crate::inbox::{InboxFuture, InboxTransport};
use crate::settlement::SettlementError;
use sui_crypto::ed25519::Ed25519PrivateKey;

/// Lower bound for the requestable timelock; mirrors `rotation::MIN_DELAY_MS`.
pub const MIN_DELAY_MS: u64 = 600_000; // 10 minutes
/// Upper bound for the requestable timelock; mirrors `rotation::MAX_DELAY_MS`.
pub const MAX_DELAY_MS: u64 = 604_800_000; // 7 days
/// Conventional request delay; mirrors `rotation::DEFAULT_DELAY_MS`.
pub const DEFAULT_DELAY_MS: u64 = 86_400_000; // 24 hours
/// Claimable lifetime of a pre-authorization; mirrors `rotation::CLAIM_WINDOW_MS`.
pub const CLAIM_WINDOW_MS: u64 = 604_800_000; // 7 days
/// Previous-owner acceptance window; mirrors `rotation::PREV_GRACE_MS`.
pub const PREV_GRACE_MS: u64 = 600_000; // 10 minutes

/// BCS encoding of the `rotation::StateKey` unit struct: no fields, no bytes.
pub const STATE_KEY_BCS: &[u8] = &[];

/// Authority record for one guarded object, mirrored from
/// `rotation::RotationState`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RotationState {
    /// The address whose settle/withdraw calls are authorized right now.
    pub authority: Address,
    /// Previous authority, still authorized until its grace deadline.
    pub prev: Option<PreviousAuthority>,
    /// Pre-authorized next authority and the earliest claim time.
    pub pending: Option<PendingRotation>,
}

/// Previous authority and its grace deadline, mirrored from
/// `rotation::PreviousAuthority`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PreviousAuthority {
    pub addr: Address,
    pub until_ms: u64,
}

/// One pre-authorization, mirrored from `rotation::PendingRotation`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PendingRotation {
    pub to: Address,
    pub eligible_at_ms: u64,
}

/// Returns the `rotation::StateKey` type tag under `package`.
fn state_key_type_tag(package: Address) -> TypeTag {
    // StructTag's fields are private; the string form parses into the same
    // canonical tag the Move module declares.
    let tag: StructTag = format!("{package}::rotation::StateKey")
        .parse()
        .expect("static type tag");
    TypeTag::Struct(Box::new(tag))
}

/// Returns the dynamic-field object id holding `RotationState` on `parent`.
///
/// `parent` is the shared Registry or Vault object id; `package` is the
/// deployed Move package id. Mirrors `rotation::state_key()`.
pub fn rotation_state_field_id(parent: Address, package: Address) -> Address {
    parent.derive_dynamic_child_id(&state_key_type_tag(package), STATE_KEY_BCS)
}

/// Decodes a [`RotationState`] from raw `Field<StateKey, RotationState>` BCS.
pub fn parse_rotation_state(field_bcs: &[u8]) -> Result<RotationState, SettlementError> {
    #[derive(serde::Deserialize)]
    #[allow(dead_code)]
    struct FieldBcs {
        id: Address,
        value: RotationState,
    }
    let field: FieldBcs = bcs::from_bytes(field_bcs)
        .map_err(|e| SettlementError::Parse(format!("rotation state BCS: {e}")))?;
    Ok(field.value)
}

/// The node's role in a rotation, derived from chain state (the table in
/// docs/src/cap-rotation.md).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Role {
    /// The configured signer key is the authority: settle with it.
    Active,
    /// The staged key is pre-authorized; the timelock has not elapsed.
    WaitingClaim { eligible_at_ms: u64 },
    /// The staged key may claim now.
    ClaimReady,
    /// The staged key is the authority; the previous one is still in grace.
    Adopt { prev_until_ms: u64 },
    /// The staged key is the authority and no previous authority remains.
    StagedIsAuthority,
    /// The authority matches neither configured key: fail closed, loudly.
    Misconfigured,
}

/// Derives the node's role from one object's rotation state.
///
/// `active` is the address of the configured signer key, `staged` the staged
/// rotation key. Claim-window edges mirror the contract: a claim is allowed
/// while `eligible_at_ms <= now <= eligible_at_ms + CLAIM_WINDOW_MS`, and the
/// previous authority acts while `now <= prev_until_ms`.
pub fn derive_role(state: &RotationState, now_ms: u64, active: &Address, staged: &Address) -> Role {
    if state.authority == *staged {
        return match &state.prev {
            Some(prev) if prev.addr == *active && now_ms <= prev.until_ms => Role::Adopt {
                prev_until_ms: prev.until_ms,
            },
            _ => Role::StagedIsAuthority,
        };
    }
    if state.authority == *active {
        if let Some(pending) = &state.pending {
            if pending.to == *staged {
                if now_ms < pending.eligible_at_ms {
                    return Role::WaitingClaim {
                        eligible_at_ms: pending.eligible_at_ms,
                    };
                }
                if now_ms <= pending.eligible_at_ms.saturating_add(CLAIM_WINDOW_MS) {
                    return Role::ClaimReady;
                }
            }
        }
        return Role::Active;
    }
    Role::Misconfigured
}

/// The action a watcher poll should take, folded from both objects' roles.
///
/// The node drives both rotations through single PTBs, so the plan acts only
/// when both objects agree; any disagreement (a partial entombment or claim)
/// is a [`Plan::Misconfigured`] refusal, not a guess.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Nothing to do (both idle, or waiting on the timelock).
    Idle,
    /// Submit the claim PTB with the staged key.
    Claim,
    /// Swap the in-memory signers to the staged key (and then retire).
    Adopt,
    /// Submit the retire PTB for both objects.
    Retire,
    /// Roles disagree or match neither key: refuse and log loudly.
    Misconfigured,
}

/// Folds both objects' roles into one plan at `now_ms`.
pub fn plan(
    settlement: &RotationState,
    vault: &RotationState,
    now_ms: u64,
    active: &Address,
    staged: &Address,
) -> Plan {
    let settlement_role = derive_role(settlement, now_ms, active, staged);
    let vault_role = derive_role(vault, now_ms, active, staged);
    match (&settlement_role, &vault_role) {
        (Role::Misconfigured, _) | (_, Role::Misconfigured) => Plan::Misconfigured,
        (Role::ClaimReady, Role::ClaimReady) => Plan::Claim,
        (Role::Adopt { .. }, Role::Adopt { .. }) => Plan::Adopt,
        (Role::StagedIsAuthority, Role::StagedIsAuthority) => {
            if settlement.prev.is_some() || vault.prev.is_some() {
                Plan::Retire
            } else {
                Plan::Idle
            }
        }
        (Role::Active, Role::Active) | (Role::WaitingClaim { .. }, Role::WaitingClaim { .. }) => {
            Plan::Idle
        }
        _ => Plan::Misconfigured,
    }
}

/// Executes one planned rotation step.
///
/// Mocked in tests; the production executor lives in the node crate, where it
/// wraps the shared settlement/bridge signers.
pub trait RotationExecutor: Send + Sync + 'static {
    /// Submits `claim_rotation` for both objects in one PTB, signed by the
    /// staged key (the pending target).
    fn claim<'a>(
        &'a self,
        staged_key: &'a Ed25519PrivateKey,
    ) -> InboxFuture<'a, Result<(), SettlementError>>;

    /// Swaps the in-memory signers (settlement and bridge) to the staged key
    /// and updates the shared address snapshot. Idempotent: a crash-restart
    /// with the swap already applied is a no-op.
    fn adopt<'a>(
        &'a self,
        staged_key: &'a Ed25519PrivateKey,
    ) -> InboxFuture<'a, Result<(), SettlementError>>;

    /// Submits `retire_previous` for both objects in one PTB, signed by the
    /// staged key (the current authority after the claim).
    fn retire_previous<'a>(
        &'a self,
        staged_key: &'a Ed25519PrivateKey,
    ) -> InboxFuture<'a, Result<(), SettlementError>>;
}

/// The key the rotation watcher adopted, shared with signer sources that
/// come up after the swap.
///
/// A relayer that starts while the node is already on the staged key (its
/// setup was crash-looping, or the bridge task lost the race with the
/// adopt) must not reload the configured key file — that file still holds
/// the previous key, which the on-chain gate rejects once the grace window
/// closes or the previous authority is retired. Every signer constructor
/// that takes a holder prefers it over the file.
#[derive(Clone, Default)]
pub struct AdoptedKey(Arc<RwLock<Option<Ed25519PrivateKey>>>);

impl AdoptedKey {
    /// Creates an empty holder.
    pub fn new() -> Self {
        Self(Arc::new(RwLock::new(None)))
    }

    /// Records the adopted key. Idempotent; the latest adopt wins.
    pub fn set(&self, key: Ed25519PrivateKey) {
        *self.0.write().expect("adopted key holder") = Some(key);
    }

    /// Returns the adopted key, if any.
    pub fn get(&self) -> Option<Ed25519PrivateKey> {
        self.0.read().expect("adopted key holder").clone()
    }
}

/// Records when the staged key was adopted, so the retire step waits out one
/// quiet interval before closing the previous authority's window.
///
/// The quiet interval approximates the in-flight drain check (signed settle
/// transactions land within a commit wait and bounded retries); the real
/// backstop is `PREV_GRACE_MS` on-chain, which outlives both.
#[derive(Debug)]
pub struct AdoptionGuard {
    adopted_at: std::sync::Mutex<Option<std::time::Instant>>,
    retire_quiet: Duration,
}

impl AdoptionGuard {
    /// Creates a guard that allows retiring `retire_quiet` after adoption.
    pub fn new(retire_quiet: Duration) -> Self {
        Self {
            adopted_at: std::sync::Mutex::new(None),
            retire_quiet,
        }
    }

    /// Marks the staged key as adopted now.
    pub fn mark_adopted(&self) {
        *self.adopted_at.lock().expect("adoption guard") = Some(std::time::Instant::now());
    }

    /// Whether the staged key was adopted (at any point).
    pub fn adopted(&self) -> bool {
        self.adopted_at.lock().expect("adoption guard").is_some()
    }

    /// Whether adoption happened at least `retire_quiet` ago.
    pub fn retire_ready(&self) -> bool {
        match *self.adopted_at.lock().expect("adoption guard") {
            Some(at) => at.elapsed() >= self.retire_quiet,
            None => false,
        }
    }
}

/// What one [`apply`] step did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Nothing to do.
    Idle,
    /// The claim PTB was submitted.
    Claimed,
    /// The signers were swapped to the staged key.
    Adopted,
    /// The retire PTB was submitted.
    Retired,
    /// The plan is not executable (missing staged key, misconfiguration).
    Refused,
}

/// Executes at most one step of `plan`, pacing retire behind the guard.
pub async fn apply(
    plan: &Plan,
    staged_key: Option<&Ed25519PrivateKey>,
    executor: &dyn RotationExecutor,
    guard: &AdoptionGuard,
) -> Result<Action, SettlementError> {
    match plan {
        Plan::Idle => Ok(Action::Idle),
        // The chain enforces fail-closed (the stale key cannot settle); the
        // caller logs loudly so the operator sees the misconfiguration.
        Plan::Misconfigured => Ok(Action::Refused),
        Plan::Claim => match staged_key {
            None => Ok(Action::Refused),
            Some(key) => {
                executor.claim(key).await?;
                Ok(Action::Claimed)
            }
        },
        Plan::Adopt => {
            if guard.adopted() {
                if guard.retire_ready() {
                    retire(staged_key, executor).await
                } else {
                    Ok(Action::Idle)
                }
            } else {
                adopt(staged_key, executor, guard).await
            }
        }
        Plan::Retire => {
            if guard.adopted() {
                retire(staged_key, executor).await
            } else {
                // Crash-restart mid-window: swap first, retire next poll.
                adopt(staged_key, executor, guard).await
            }
        }
    }
}

async fn adopt(
    staged_key: Option<&Ed25519PrivateKey>,
    executor: &dyn RotationExecutor,
    guard: &AdoptionGuard,
) -> Result<Action, SettlementError> {
    match staged_key {
        None => Ok(Action::Refused),
        Some(key) => {
            executor.adopt(key).await?;
            guard.mark_adopted();
            Ok(Action::Adopted)
        }
    }
}

async fn retire(
    staged_key: Option<&Ed25519PrivateKey>,
    executor: &dyn RotationExecutor,
) -> Result<Action, SettlementError> {
    match staged_key {
        None => Ok(Action::Refused),
        Some(key) => {
            executor.retire_previous(key).await?;
            Ok(Action::Retired)
        }
    }
}

/// Reads both rotation states and the clock over the transport, and folds
/// them into a plan.
///
/// `staged` is `None` when no rotation key is staged: plans stay idle (the
/// misconfiguration case still surfaces through [`Role::Misconfigured`] in
/// [`Self::roles`]).
pub struct RotationWatcher<T: InboxTransport> {
    transport: T,
    package: Address,
    registry: Address,
    vault: Address,
    active: Address,
    staged: Option<Address>,
}

/// One poll's read: both states and the clock timestamp.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RotationStatus {
    pub settlement: RotationState,
    pub vault: RotationState,
    pub now_ms: u64,
}

impl<T: InboxTransport> RotationWatcher<T> {
    /// Creates a watcher for the entombed caps on `registry` and `vault`.
    pub fn new(
        transport: T,
        package: Address,
        registry: Address,
        vault: Address,
        active: Address,
        staged: Option<Address>,
    ) -> Self {
        Self {
            transport,
            package,
            registry,
            vault,
            active,
            staged,
        }
    }

    /// Reads both rotation states and the clock in three `get_object`s.
    pub async fn status(&self) -> Result<RotationStatus, crate::inbox::InboxError> {
        let settlement_bytes = self
            .transport
            .object_contents(&rotation_state_field_id(self.registry, self.package))
            .await?;
        let vault_bytes = self
            .transport
            .object_contents(&rotation_state_field_id(self.vault, self.package))
            .await?;
        let clock_bytes = self.transport.object_contents(&SUI_CLOCK_ID).await?;
        let settlement = parse_rotation_state(&settlement_bytes)
            .map_err(|e| crate::inbox::InboxError::Parse(e.to_string()))?;
        let vault = parse_rotation_state(&vault_bytes)
            .map_err(|e| crate::inbox::InboxError::Parse(e.to_string()))?;
        Ok(RotationStatus {
            settlement,
            vault,
            now_ms: parse_clock(&clock_bytes)?,
        })
    }

    /// The per-object roles for the configured keys.
    pub fn roles(&self, status: &RotationStatus) -> (Role, Role) {
        let Some(staged) = self.staged else {
            return (Role::Misconfigured, Role::Misconfigured);
        };
        (
            derive_role(&status.settlement, status.now_ms, &self.active, &staged),
            derive_role(&status.vault, status.now_ms, &self.active, &staged),
        )
    }

    /// Folds the current chain state into a plan. Idle while no staged key
    /// is configured.
    pub async fn plan(&self) -> Result<Plan, crate::inbox::InboxError> {
        let Some(staged) = self.staged else {
            return Ok(Plan::Idle);
        };
        let status = self.status().await?;
        Ok(plan(
            &status.settlement,
            &status.vault,
            status.now_ms,
            &self.active,
            &staged,
        ))
    }

    /// Whether `parent` still needs the entombment ceremony: its
    /// rotation-state dynamic field does not exist (a deployment upgraded
    /// from a pre-rotation package).
    ///
    /// Transport errors propagate — an unreadable chain state must never be
    /// read as "needs entombment", because the ceremony moves an owned
    /// object.
    pub async fn entombment_needed(
        &self,
        parent: Address,
    ) -> Result<bool, crate::inbox::InboxError> {
        match self
            .transport
            .object_contents(&rotation_state_field_id(parent, self.package))
            .await
        {
            Err(crate::inbox::InboxError::NotFound(_)) => Ok(true),
            Err(e) => Err(e),
            Ok(_) => Ok(false),
        }
    }
}

/// Objects one rotation PTB touches.
#[derive(Debug, Clone)]
pub struct RotationTargets {
    /// Deployed Move package id (contains the `settlement`, `bridge`, and
    /// `rotation` modules).
    pub package: Address,
    /// Shared settlement Registry object id.
    pub registry: Address,
    /// Shared bridge Vault object id.
    pub vault: Address,
}

fn module_function(
    package: Address,
    module: &str,
    name: &str,
) -> Result<Function, SettlementError> {
    Ok(Function::new(
        package,
        Identifier::new(module).map_err(|e| SettlementError::Parse(e.to_string()))?,
        Identifier::new(name).map_err(|e| SettlementError::Parse(e.to_string()))?,
    ))
}

/// Builds, signs, and confirms a PTB through `builder_fn`.
async fn submit_rotation_tx(
    rpc_url: &str,
    key: &Ed25519PrivateKey,
    label: &str,
    build: impl FnOnce(&mut TransactionBuilder),
) -> Result<(), SettlementError> {
    let sender = key.public_key().derive_address();
    let mut builder = TransactionBuilder::new();
    builder.set_sender(sender);
    build(&mut builder);
    let mut rpc = sui_rpc::Client::new(rpc_url).map_err(|e| SettlementError::Rpc(e.to_string()))?;
    let tx = builder
        .build(&mut rpc)
        .await
        .map_err(|e| SettlementError::Build(format!("{label}: {e}")))?;
    let mut rpc2 =
        sui_rpc::Client::new(rpc_url).map_err(|e| SettlementError::Rpc(e.to_string()))?;
    crate::settlement::sign_and_execute(
        &mut rpc2,
        key,
        tx,
        label,
        crate::settlement::DEPLOY_TX_WAIT,
    )
    .await?;
    Ok(())
}

/// Claims both rotations in one PTB, signed by the staged key (the pending
/// target). Claim-only-by-target and the clock-anchored window are enforced
/// on-chain; a replay aborts there.
pub async fn submit_claim(
    rpc_url: &str,
    key: &Ed25519PrivateKey,
    targets: &RotationTargets,
) -> Result<(), SettlementError> {
    submit_rotation_tx(rpc_url, key, "rotation claim", |builder| {
        let registry = builder.object(ObjectInput::new(targets.registry));
        let clock = builder.object(ObjectInput::new(SUI_CLOCK_ID));
        let vault = builder.object(ObjectInput::new(targets.vault));
        let clock2 = builder.object(ObjectInput::new(SUI_CLOCK_ID));
        builder.move_call(
            module_function(targets.package, "settlement", "claim_rotation").expect("static name"),
            vec![registry, clock],
        );
        builder.move_call(
            module_function(targets.package, "bridge", "claim_rotation").expect("static name"),
            vec![vault, clock2],
        );
    })
    .await
}

/// Retires both previous authorities in one PTB, signed by the current
/// authority (the staged key after the claim).
pub async fn submit_retire_previous(
    rpc_url: &str,
    key: &Ed25519PrivateKey,
    targets: &RotationTargets,
) -> Result<(), SettlementError> {
    submit_rotation_tx(rpc_url, key, "rotation retire", |builder| {
        let registry = builder.object(ObjectInput::new(targets.registry));
        let vault = builder.object(ObjectInput::new(targets.vault));
        builder.move_call(
            module_function(targets.package, "settlement", "retire_previous").expect("static name"),
            vec![registry],
        );
        builder.move_call(
            module_function(targets.package, "bridge", "retire_previous").expect("static name"),
            vec![vault],
        );
    })
    .await
}

/// Pre-authorizes `to` on both objects in one PTB, signed by the current
/// authority. Both timelocks start at the same clock, so the two rotations
/// cannot drift apart.
pub async fn submit_request(
    rpc_url: &str,
    key: &Ed25519PrivateKey,
    targets: &RotationTargets,
    to: Address,
    delay_ms: u64,
) -> Result<(), SettlementError> {
    if !(MIN_DELAY_MS..=MAX_DELAY_MS).contains(&delay_ms) {
        return Err(SettlementError::Parse(format!(
            "rotation delay {delay_ms} ms outside [{MIN_DELAY_MS}, {MAX_DELAY_MS}]"
        )));
    }
    submit_rotation_tx(rpc_url, key, "rotation request", |builder| {
        let registry = builder.object(ObjectInput::new(targets.registry));
        let clock = builder.object(ObjectInput::new(SUI_CLOCK_ID));
        let to_arg = builder.pure(&to);
        let delay_arg = builder.pure(&delay_ms);
        let vault = builder.object(ObjectInput::new(targets.vault));
        let clock2 = builder.object(ObjectInput::new(SUI_CLOCK_ID));
        let to_arg2 = builder.pure(&to);
        let delay_arg2 = builder.pure(&delay_ms);
        builder.move_call(
            module_function(targets.package, "settlement", "request_rotation")
                .expect("static name"),
            vec![registry, clock, to_arg, delay_arg],
        );
        builder.move_call(
            module_function(targets.package, "bridge", "request_rotation").expect("static name"),
            vec![vault, clock2, to_arg2, delay_arg2],
        );
    })
    .await
}

/// Entombs the `SettlerCap` into the Registry: the migration ceremony for a
/// deployment upgraded from a pre-rotation package (fresh deploys entomb in
/// [`crate::settlement::Settlement::deploy`]).
///
/// Signed by the cap owner, who becomes the initial rotation authority.
/// Aborts on-chain with `E_ALREADY_ENTOMBED` when a concurrent ceremony won
/// the race — callers treat that as done.
///
/// # Errors
///
/// Rpc, Build, or Execution errors from the submit path; an on-chain abort
/// (`E_ALREADY_ENTOMBED`) arrives as [`SettlementError::Execution`].
///
/// # Panics
///
/// Only if the hard-coded module/function names stop being valid Sui
/// identifiers, which is a compile-time property.
pub async fn submit_entomb_settler(
    rpc_url: &str,
    key: &Ed25519PrivateKey,
    package: Address,
    registry: Address,
    settler_cap: Address,
) -> Result<(), SettlementError> {
    submit_rotation_tx(rpc_url, key, "entomb settler cap", |builder| {
        let cap = builder.object(ObjectInput::new(settler_cap));
        let registry = builder.object(ObjectInput::new(registry));
        builder.move_call(
            module_function(package, "settlement", "entomb_settler_cap").expect("static name"),
            vec![cap, registry],
        );
    })
    .await
}

/// Entombs the `BridgeCap` into the Vault — the Vault-side twin of
/// [`submit_entomb_settler`].
///
/// # Errors
///
/// Rpc, Build, or Execution errors from the submit path; an on-chain abort
/// (`E_ALREADY_ENTOMBED`) arrives as [`SettlementError::Execution`].
///
/// # Panics
///
/// Only if the hard-coded module/function names stop being valid Sui
/// identifiers, which is a compile-time property.
pub async fn submit_entomb_bridge(
    rpc_url: &str,
    key: &Ed25519PrivateKey,
    package: Address,
    vault: Address,
    bridge_cap: Address,
) -> Result<(), SettlementError> {
    submit_rotation_tx(rpc_url, key, "entomb bridge cap", |builder| {
        let cap = builder.object(ObjectInput::new(bridge_cap));
        let vault = builder.object(ObjectInput::new(vault));
        builder.move_call(
            module_function(package, "bridge", "entomb_bridge_cap").expect("static name"),
            vec![cap, vault],
        );
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALICE: Address = Address::new([0xAA; 32]);
    const BOB: Address = Address::new([0xBB; 32]);
    const CAROL: Address = Address::new([0xCC; 32]);

    /// Mirrors `sui::clock::Clock { id: UID, timestamp_ms: u64 }`.
    #[derive(serde::Serialize)]
    #[allow(dead_code)]
    struct ClockMirror {
        id: Address,
        timestamp_ms: u64,
    }

    /// BCS mirrors of the Move structs (field order is the wire format).
    #[derive(serde::Serialize)]
    struct FieldMirror {
        #[allow(dead_code)]
        id: Address,
        value: RotationStateMirror,
    }
    #[derive(serde::Serialize)]
    struct RotationStateMirror {
        #[allow(dead_code)]
        authority: Address,
        prev: Option<PrevMirror>,
        pending: Option<PendingMirror>,
    }
    #[derive(serde::Serialize)]
    struct PrevMirror {
        #[allow(dead_code)]
        addr: Address,
        until_ms: u64,
    }
    #[derive(serde::Serialize)]
    struct PendingMirror {
        #[allow(dead_code)]
        to: Address,
        eligible_at_ms: u64,
    }

    fn field_bcs(state: RotationStateMirror) -> Vec<u8> {
        bcs::to_bytes(&FieldMirror {
            id: Address::new([0x11; 32]),
            value: state,
        })
        .unwrap()
    }

    fn state(
        authority: Address,
        prev: Option<(Address, u64)>,
        pending: Option<(Address, u64)>,
    ) -> RotationState {
        RotationState {
            authority,
            prev: prev.map(|(addr, until_ms)| PreviousAuthority { addr, until_ms }),
            pending: pending.map(|(to, eligible_at_ms)| PendingRotation { to, eligible_at_ms }),
        }
    }

    #[test]
    fn parse_rotation_state_roundtrips_the_bcs_layout() {
        let mirror = RotationStateMirror {
            authority: BOB,
            prev: Some(PrevMirror {
                addr: ALICE,
                until_ms: 1_234,
            }),
            pending: Some(PendingMirror {
                to: CAROL,
                eligible_at_ms: 5_678,
            }),
        };
        let parsed = parse_rotation_state(&field_bcs(mirror)).unwrap();
        assert_eq!(
            parsed,
            state(BOB, Some((ALICE, 1_234)), Some((CAROL, 5_678)))
        );
    }

    #[test]
    fn parse_rotation_state_reads_empty_option_fields() {
        let mirror = RotationStateMirror {
            authority: ALICE,
            prev: None,
            pending: None,
        };
        assert_eq!(
            parse_rotation_state(&field_bcs(mirror)).unwrap(),
            state(ALICE, None, None)
        );
    }

    #[test]
    fn parse_rotation_state_rejects_truncated_bcs() {
        let full = field_bcs(RotationStateMirror {
            authority: ALICE,
            prev: None,
            pending: None,
        });
        assert!(parse_rotation_state(&full[..full.len() - 1]).is_err());
        assert!(parse_rotation_state(&[0u8; 8]).is_err());
    }

    #[test]
    fn field_id_is_deterministic_and_distinct_per_parent_and_package() {
        let package = Address::new([0x01; 32]);
        let registry = Address::new([0x02; 32]);
        let vault = Address::new([0x03; 32]);

        let id = rotation_state_field_id(registry, package);
        assert_eq!(id, rotation_state_field_id(registry, package));
        assert_ne!(id, rotation_state_field_id(vault, package));
        assert_ne!(
            id,
            rotation_state_field_id(registry, Address::new([0x09; 32]))
        );
    }

    // ---------- derive_role (the table in docs/src/cap-rotation.md) ----------

    #[test]
    fn role_is_active_when_the_signer_key_is_the_authority() {
        let s = state(ALICE, None, None);
        assert_eq!(derive_role(&s, 1_000, &ALICE, &BOB), Role::Active);
    }

    #[test]
    fn role_waits_for_the_timelock() {
        let s = state(ALICE, None, Some((BOB, 5_000)));
        assert_eq!(
            derive_role(&s, 4_999, &ALICE, &BOB),
            Role::WaitingClaim {
                eligible_at_ms: 5_000
            }
        );
    }

    #[test]
    fn role_is_claim_ready_through_the_window() {
        let s = state(ALICE, None, Some((BOB, 5_000)));
        assert_eq!(derive_role(&s, 5_000, &ALICE, &BOB), Role::ClaimReady);
        assert_eq!(
            derive_role(&s, 5_000 + CLAIM_WINDOW_MS, &ALICE, &BOB),
            Role::ClaimReady
        );
    }

    #[test]
    fn an_expired_preauthorization_leaves_the_authority_in_charge() {
        let s = state(ALICE, None, Some((BOB, 5_000)));
        assert_eq!(
            derive_role(&s, 5_000 + CLAIM_WINDOW_MS + 1, &ALICE, &BOB),
            Role::Active
        );
    }

    #[test]
    fn a_pending_request_to_a_third_party_changes_nothing() {
        let s = state(ALICE, None, Some((CAROL, 0)));
        assert_eq!(derive_role(&s, 1_000, &ALICE, &BOB), Role::Active);
    }

    #[test]
    fn role_adopts_while_the_previous_authority_is_in_grace() {
        let s = state(BOB, Some((ALICE, 9_000)), None);
        assert_eq!(
            derive_role(&s, 9_000, &ALICE, &BOB),
            Role::Adopt {
                prev_until_ms: 9_000
            }
        );
    }

    #[test]
    fn role_is_settled_once_grace_expires() {
        let s = state(BOB, Some((ALICE, 9_000)), None);
        assert_eq!(
            derive_role(&s, 9_001, &ALICE, &BOB),
            Role::StagedIsAuthority
        );
    }

    #[test]
    fn role_is_settled_without_a_previous_record() {
        let s = state(BOB, None, None);
        assert_eq!(derive_role(&s, 0, &ALICE, &BOB), Role::StagedIsAuthority);
    }

    #[test]
    fn a_third_party_previous_record_does_not_block_settled_role() {
        let s = state(BOB, Some((CAROL, 9_000)), None);
        assert_eq!(derive_role(&s, 0, &ALICE, &BOB), Role::StagedIsAuthority);
    }

    #[test]
    fn an_unrecognized_authority_is_misconfigured() {
        let s = state(CAROL, None, None);
        assert_eq!(derive_role(&s, 1_000, &ALICE, &BOB), Role::Misconfigured);
    }

    // ---------- plan folding ----------

    #[test]
    fn plan_claims_only_when_both_objects_agree() {
        let ready = state(ALICE, None, Some((BOB, 0)));
        let idle = state(ALICE, None, None);
        let now = 1;
        assert_eq!(plan(&ready, &ready, now, &ALICE, &BOB), Plan::Claim);
        assert_eq!(plan(&ready, &idle, now, &ALICE, &BOB), Plan::Misconfigured);
    }

    #[test]
    fn plan_adopts_only_when_both_objects_agree() {
        let adopted = state(BOB, Some((ALICE, 9_000)), None);
        let settled = state(BOB, None, None);
        assert_eq!(plan(&adopted, &adopted, 0, &ALICE, &BOB), Plan::Adopt);
        assert_eq!(
            plan(&adopted, &settled, 0, &ALICE, &BOB),
            Plan::Misconfigured
        );
    }

    #[test]
    fn plan_retires_when_a_previous_record_remains() {
        let with_prev = state(BOB, Some((ALICE, 9_000)), None);
        let clean = state(BOB, None, None);
        assert_eq!(
            plan(&with_prev, &with_prev, 9_001, &ALICE, &BOB),
            Plan::Retire
        );
        // Either side's stale record is worth clearing.
        assert_eq!(plan(&clean, &with_prev, 9_001, &ALICE, &BOB), Plan::Retire);
        assert_eq!(plan(&clean, &clean, 9_001, &ALICE, &BOB), Plan::Idle);
    }

    #[test]
    fn plan_stays_idle_when_waiting_or_settled() {
        let waiting = state(ALICE, None, Some((BOB, 9_000)));
        let settled = state(BOB, None, None);
        assert_eq!(plan(&waiting, &waiting, 0, &ALICE, &BOB), Plan::Idle);
        assert_eq!(plan(&settled, &settled, 9_001, &ALICE, &BOB), Plan::Idle);
    }

    #[test]
    fn plan_refuses_a_misconfigured_authority() {
        let foreign = state(CAROL, None, None);
        let idle = state(ALICE, None, None);
        assert_eq!(
            plan(&foreign, &foreign, 0, &ALICE, &BOB),
            Plan::Misconfigured
        );
        assert_eq!(plan(&foreign, &idle, 0, &ALICE, &BOB), Plan::Misconfigured);
    }

    // ---------- apply pacing ----------

    /// Records executed steps; fail-closed on unexpected staged-key access.
    struct MockExecutor {
        steps: std::sync::Mutex<Vec<&'static str>>,
    }

    impl MockExecutor {
        fn new() -> Self {
            Self {
                steps: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn log(&self, step: &'static str) {
            self.steps.lock().unwrap().push(step);
        }

        fn steps(&self) -> Vec<&'static str> {
            self.steps.lock().unwrap().clone()
        }
    }

    impl RotationExecutor for MockExecutor {
        fn claim<'a>(
            &'a self,
            _staged_key: &'a Ed25519PrivateKey,
        ) -> InboxFuture<'a, Result<(), SettlementError>> {
            Box::pin(async move {
                self.log("claim");
                Ok(())
            })
        }

        fn adopt<'a>(
            &'a self,
            _staged_key: &'a Ed25519PrivateKey,
        ) -> InboxFuture<'a, Result<(), SettlementError>> {
            Box::pin(async move {
                self.log("adopt");
                Ok(())
            })
        }

        fn retire_previous<'a>(
            &'a self,
            _staged_key: &'a Ed25519PrivateKey,
        ) -> InboxFuture<'a, Result<(), SettlementError>> {
            Box::pin(async move {
                self.log("retire");
                Ok(())
            })
        }
    }

    fn staged_key() -> Ed25519PrivateKey {
        Ed25519PrivateKey::new([0x5E; 32])
    }

    #[tokio::test]
    async fn apply_executes_one_step_per_plan() {
        let executor = MockExecutor::new();
        // A zero quiet interval lets retire run on the next poll; the paced
        // path is covered by `apply_after_a_crash_adopts_before_retiring`.
        let guard = AdoptionGuard::new(Duration::ZERO);
        let key = staged_key();

        assert_eq!(
            apply(&Plan::Claim, Some(&key), &executor, &guard)
                .await
                .unwrap(),
            Action::Claimed
        );
        assert_eq!(
            apply(&Plan::Adopt, Some(&key), &executor, &guard)
                .await
                .unwrap(),
            Action::Adopted
        );
        assert_eq!(
            apply(&Plan::Adopt, Some(&key), &executor, &guard)
                .await
                .unwrap(),
            Action::Retired
        );
        assert_eq!(
            apply(&Plan::Idle, Some(&key), &executor, &guard)
                .await
                .unwrap(),
            Action::Idle
        );
        assert_eq!(executor.steps(), vec!["claim", "adopt", "retire"]);
    }

    #[tokio::test]
    async fn apply_refuses_to_claim_without_a_staged_key() {
        let executor = MockExecutor::new();
        let guard = AdoptionGuard::new(Duration::ZERO);

        assert_eq!(
            apply(&Plan::Claim, None, &executor, &guard).await.unwrap(),
            Action::Refused
        );
        assert_eq!(
            apply(&Plan::Adopt, None, &executor, &guard).await.unwrap(),
            Action::Refused
        );
        assert!(executor.steps().is_empty());
    }

    #[tokio::test]
    async fn apply_refuses_a_misconfigured_plan_without_touching_signers() {
        let executor = MockExecutor::new();
        let guard = AdoptionGuard::new(Duration::ZERO);
        let key = staged_key();

        assert_eq!(
            apply(&Plan::Misconfigured, Some(&key), &executor, &guard)
                .await
                .unwrap(),
            Action::Refused
        );
        assert!(executor.steps().is_empty());
    }

    #[tokio::test]
    async fn apply_after_a_crash_adopts_before_retiring() {
        // Fresh guard (restart) with the plan already past the claim: the
        // first step must swap the signers, not retire.
        let executor = MockExecutor::new();
        let guard = AdoptionGuard::new(Duration::from_secs(3600));
        let key = staged_key();

        assert_eq!(
            apply(&Plan::Retire, Some(&key), &executor, &guard)
                .await
                .unwrap(),
            Action::Adopted
        );
        assert_eq!(executor.steps(), vec!["adopt"]);
        assert!(guard.adopted());
        assert!(!guard.retire_ready());
    }

    // ---------- watcher over the transport ----------

    /// In-memory transport serving objects by id, mirroring the real
    /// `get_object` shape.
    #[derive(Default)]
    struct MockTransport {
        objects: std::collections::BTreeMap<Address, Vec<u8>>,
        /// Reads of these ids fail with a non-`NotFound` transport error.
        rpc_error_for: Option<Address>,
    }

    impl InboxTransport for MockTransport {
        fn object_contents<'a>(
            &'a self,
            id: &'a Address,
        ) -> InboxFuture<'a, Result<Vec<u8>, crate::inbox::InboxError>> {
            Box::pin(async move {
                if self.rpc_error_for == Some(*id) {
                    return Err(crate::inbox::InboxError::Rpc(
                        "injected transport failure".into(),
                    ));
                }
                self.objects
                    .get(id)
                    .cloned()
                    .ok_or_else(|| crate::inbox::InboxError::NotFound(format!("no object {id}")))
            })
        }

        fn field_contents<'a>(
            &'a self,
            _table: &'a Address,
            _key: u64,
        ) -> InboxFuture<'a, Result<Option<Vec<u8>>, crate::inbox::InboxError>> {
            Box::pin(async move { Ok(None) })
        }
    }

    fn mock_chain(
        settlement: &RotationState,
        vault: &RotationState,
        now_ms: u64,
    ) -> (MockTransport, Address, Address, Address) {
        let package = Address::new([0x21; 32]);
        let registry_id = Address::new([0x22; 32]);
        let vault_id = Address::new([0x23; 32]);
        let mut transport = MockTransport::default();
        transport.objects.insert(
            rotation_state_field_id(registry_id, package),
            field_bcs(RotationStateMirror {
                authority: settlement.authority,
                prev: settlement.prev.as_ref().map(|p| PrevMirror {
                    addr: p.addr,
                    until_ms: p.until_ms,
                }),
                pending: settlement.pending.as_ref().map(|p| PendingMirror {
                    to: p.to,
                    eligible_at_ms: p.eligible_at_ms,
                }),
            }),
        );
        transport.objects.insert(
            rotation_state_field_id(vault_id, package),
            field_bcs(RotationStateMirror {
                authority: vault.authority,
                prev: vault.prev.as_ref().map(|p| PrevMirror {
                    addr: p.addr,
                    until_ms: p.until_ms,
                }),
                pending: vault.pending.as_ref().map(|p| PendingMirror {
                    to: p.to,
                    eligible_at_ms: p.eligible_at_ms,
                }),
            }),
        );
        transport.objects.insert(
            SUI_CLOCK_ID,
            bcs::to_bytes(&ClockMirror {
                id: Address::new([0; 32]),
                timestamp_ms: now_ms,
            })
            .unwrap(),
        );
        (transport, package, registry_id, vault_id)
    }

    #[tokio::test]
    async fn watcher_reads_both_states_and_the_clock() {
        let (transport, package, registry, vault) = mock_chain(
            &state(ALICE, None, Some((BOB, 7_000))),
            &state(ALICE, None, Some((BOB, 7_000))),
            6_000,
        );
        let watcher = RotationWatcher::new(transport, package, registry, vault, ALICE, Some(BOB));

        let status = watcher.status().await.unwrap();
        assert_eq!(status.now_ms, 6_000);
        assert_eq!(
            watcher.roles(&status),
            (
                Role::WaitingClaim {
                    eligible_at_ms: 7_000
                },
                Role::WaitingClaim {
                    eligible_at_ms: 7_000
                }
            )
        );
        assert_eq!(watcher.plan().await.unwrap(), Plan::Idle);
    }

    #[tokio::test]
    async fn watcher_folds_a_claimable_request_into_a_claim_plan() {
        let (transport, package, registry, vault) = mock_chain(
            &state(ALICE, None, Some((BOB, 7_000))),
            &state(ALICE, None, Some((BOB, 7_000))),
            7_000,
        );
        let watcher = RotationWatcher::new(transport, package, registry, vault, ALICE, Some(BOB));

        assert_eq!(watcher.plan().await.unwrap(), Plan::Claim);
    }

    #[tokio::test]
    async fn watcher_without_a_staged_key_stays_idle() {
        let (transport, package, registry, vault) = mock_chain(
            &state(ALICE, None, Some((BOB, 0))),
            &state(ALICE, None, Some((BOB, 0))),
            1,
        );
        let watcher = RotationWatcher::new(transport, package, registry, vault, ALICE, None);

        assert_eq!(watcher.plan().await.unwrap(), Plan::Idle);
    }

    #[tokio::test]
    async fn watcher_surfaces_transport_errors() {
        let (transport, package, registry, _vault) =
            mock_chain(&state(ALICE, None, None), &state(ALICE, None, None), 0);
        let watcher = RotationWatcher::new(
            transport,
            package,
            registry,
            Address::new([0xEE; 32]),
            ALICE,
            Some(BOB),
        );

        assert!(matches!(
            watcher.status().await,
            Err(crate::inbox::InboxError::NotFound(msg)) if msg.contains("no object")
        ));
    }

    #[test]
    fn timelock_bounds_mirror_the_move_constants() {
        // rotation.move: min 10 min, max 7 d, default 24 h, window 7 d, grace 10 min.
        assert_eq!(MIN_DELAY_MS, 600_000);
        assert_eq!(MAX_DELAY_MS, 604_800_000);
        assert_eq!(DEFAULT_DELAY_MS, 86_400_000);
        assert_eq!(CLAIM_WINDOW_MS, 604_800_000);
        assert_eq!(PREV_GRACE_MS, 600_000);
    }

    // ---------- crash drills: restart at every phase boundary ----------

    /// Chain state a drill boots from; the executor mutates it the way the
    /// real PTBs mutate the chain.
    struct ChainState {
        settlement: RotationState,
        vault: RotationState,
        now_ms: u64,
    }

    /// Executor that applies each step's on-chain effect to a shared
    /// [`ChainState`], so the drill loop can rebuild the watcher from chain
    /// state alone on every poll — exactly what a restarted node sees.
    struct ChainExecutor {
        chain: Arc<std::sync::Mutex<ChainState>>,
        log: Arc<std::sync::Mutex<Vec<&'static str>>>,
        active: Address,
        staged_addr: Address,
    }

    impl ChainExecutor {
        fn new(chain: ChainState) -> (Self, Arc<std::sync::Mutex<ChainState>>) {
            let chain = Arc::new(std::sync::Mutex::new(chain));
            let executor = Self {
                chain: Arc::clone(&chain),
                log: Arc::new(std::sync::Mutex::new(Vec::new())),
                active: ALICE,
                staged_addr: BOB,
            };
            (executor, chain)
        }

        fn steps(&self) -> Vec<&'static str> {
            self.log.lock().unwrap().clone()
        }
    }

    impl RotationExecutor for ChainExecutor {
        fn claim<'a>(
            &'a self,
            _staged_key: &'a Ed25519PrivateKey,
        ) -> InboxFuture<'a, Result<(), SettlementError>> {
            Box::pin(async move {
                let until_ms;
                {
                    let mut chain = self.chain.lock().unwrap();
                    until_ms = chain.now_ms.saturating_add(PREV_GRACE_MS);
                    let state = &mut *chain;
                    for parent in [&mut state.settlement, &mut state.vault] {
                        parent.authority = self.staged_addr;
                        parent.prev = Some(PreviousAuthority {
                            addr: self.active,
                            until_ms,
                        });
                        parent.pending = None;
                    }
                }
                self.log.lock().unwrap().push("claim");
                Ok(())
            })
        }

        fn adopt<'a>(
            &'a self,
            _staged_key: &'a Ed25519PrivateKey,
        ) -> InboxFuture<'a, Result<(), SettlementError>> {
            Box::pin(async move {
                // The real adopt swaps in-memory signers: no chain change.
                self.log.lock().unwrap().push("adopt");
                Ok(())
            })
        }

        fn retire_previous<'a>(
            &'a self,
            _staged_key: &'a Ed25519PrivateKey,
        ) -> InboxFuture<'a, Result<(), SettlementError>> {
            Box::pin(async move {
                let mut chain = self.chain.lock().unwrap();
                chain.settlement.prev = None;
                chain.vault.prev = None;
                drop(chain);
                self.log.lock().unwrap().push("retire");
                Ok(())
            })
        }
    }

    /// Drives plan→apply polls against a chain the executor mutates, until
    /// the plan goes idle or refuses. The watcher is rebuilt from chain
    /// state on every poll; `guard` and `executor` model one process
    /// lifetime (a restart constructs fresh ones, which is how the tests
    /// kill the watcher at a boundary).
    async fn drill(
        chain: &Arc<std::sync::Mutex<ChainState>>,
        executor: &ChainExecutor,
        guard: &AdoptionGuard,
        key: &Ed25519PrivateKey,
    ) -> Plan {
        let package = Address::new([0x21; 32]);
        for _ in 0..8 {
            let (transport, _, registry, vault) = {
                let snapshot = chain.lock().unwrap();
                mock_chain(&snapshot.settlement, &snapshot.vault, snapshot.now_ms)
            };
            let watcher =
                RotationWatcher::new(transport, package, registry, vault, ALICE, Some(BOB));
            let plan = watcher.plan().await.unwrap();
            if plan == Plan::Idle {
                return plan;
            }
            let action = apply(&plan, Some(key), executor, guard).await.unwrap();
            if matches!(action, Action::Idle | Action::Refused) {
                return plan;
            }
        }
        panic!("drill did not converge to an idle plan");
    }

    #[tokio::test]
    async fn drill_a_crash_before_the_claim_resumes_and_converges() {
        // Chain at boot: the pre-authorization landed and the timelock
        // elapsed. The first step of the restarted watcher must be the
        // claim; from there adopt and retire follow to the settled state.
        let chain_state = ChainState {
            settlement: state(ALICE, None, Some((BOB, 1_000))),
            vault: state(ALICE, None, Some((BOB, 1_000))),
            now_ms: 1_000,
        };
        let (executor, chain) = ChainExecutor::new(chain_state);
        let guard = AdoptionGuard::new(Duration::ZERO);
        let key = staged_key();

        let plan = drill(&chain, &executor, &guard, &key).await;
        assert_eq!(plan, Plan::Idle);
        assert_eq!(executor.steps(), vec!["claim", "adopt", "retire"]);
        let end = chain.lock().unwrap();
        assert_eq!(end.settlement.authority, BOB);
        assert_eq!(end.vault.authority, BOB);
        assert!(end.settlement.prev.is_none() && end.vault.prev.is_none());
    }

    #[tokio::test]
    async fn drill_a_crash_between_claim_and_adopt_swaps_before_retiring() {
        // Chain at boot: the claim landed, the node died before the swap.
        // The restarted watcher's first step must adopt (swap the signers),
        // never retire — retiring without the swap in place is what a
        // naive "submit retire" resume would do.
        let chain_state = ChainState {
            settlement: state(BOB, Some((ALICE, 1_000 + PREV_GRACE_MS)), None),
            vault: state(BOB, Some((ALICE, 1_000 + PREV_GRACE_MS)), None),
            now_ms: 1_001,
        };
        let (executor, chain) = ChainExecutor::new(chain_state);
        let guard = AdoptionGuard::new(Duration::ZERO);
        let key = staged_key();

        let plan = drill(&chain, &executor, &guard, &key).await;
        assert_eq!(plan, Plan::Idle);
        assert_eq!(executor.steps().first(), Some(&"adopt"));
        assert_eq!(executor.steps(), vec!["adopt", "retire"]);
        assert!(chain.lock().unwrap().settlement.prev.is_none());
    }

    #[tokio::test]
    async fn drill_a_crash_between_adopt_and_retire_loses_nothing() {
        // Chain at boot: adopt done in the crashed process (guard carries
        // the marker). The only remaining step is the retire.
        let chain_state = ChainState {
            settlement: state(BOB, Some((ALICE, 1_000 + PREV_GRACE_MS)), None),
            vault: state(BOB, Some((ALICE, 1_000 + PREV_GRACE_MS)), None),
            now_ms: 1_001,
        };
        let (executor, chain) = ChainExecutor::new(chain_state);
        let guard = AdoptionGuard::new(Duration::ZERO);
        guard.mark_adopted();
        let key = staged_key();

        let plan = drill(&chain, &executor, &guard, &key).await;
        assert_eq!(plan, Plan::Idle);
        assert_eq!(executor.steps(), vec!["retire"]);
    }

    #[tokio::test]
    async fn drill_a_half_claimed_chain_refuses_instead_of_half_adopting() {
        // Adversarial chain state (a manual single-object claim): the
        // Registry is on the staged key, the Vault still pre-authorized.
        // The plan must refuse — swapping the signers would point the
        // relayer at a key that cannot settle — and leave the chain as is.
        let chain_state = ChainState {
            settlement: state(BOB, Some((ALICE, 1_000 + PREV_GRACE_MS)), None),
            vault: state(ALICE, None, Some((BOB, 1_000))),
            now_ms: 1_001,
        };
        let (executor, chain) = ChainExecutor::new(chain_state);
        let guard = AdoptionGuard::new(Duration::ZERO);
        let key = staged_key();

        let plan = drill(&chain, &executor, &guard, &key).await;
        assert_eq!(plan, Plan::Misconfigured);
        assert!(executor.steps().is_empty());
        let end = chain.lock().unwrap();
        assert_eq!(end.vault.authority, ALICE);
        assert!(end.vault.pending.is_some());
    }

    #[tokio::test]
    async fn drill_a_node_down_through_the_whole_grace_still_converges() {
        // Chain at boot: the claim landed and the grace window already
        // expired while the node was down. The plan is Retire (stale prev
        // record), and apply must still swap the signers first.
        let chain_state = ChainState {
            settlement: state(BOB, Some((ALICE, 1_000)), None),
            vault: state(BOB, Some((ALICE, 1_000)), None),
            now_ms: 1_000 + PREV_GRACE_MS + 1,
        };
        let (executor, chain) = ChainExecutor::new(chain_state);
        let guard = AdoptionGuard::new(Duration::ZERO);
        let key = staged_key();

        let plan = drill(&chain, &executor, &guard, &key).await;
        assert_eq!(plan, Plan::Idle);
        assert_eq!(executor.steps(), vec!["adopt", "retire"]);
    }

    // ---------- entombment detection (the pre-rotation migration) ----------

    #[tokio::test]
    async fn entombment_needed_when_the_state_field_is_absent() {
        let watcher = RotationWatcher::new(
            MockTransport::default(),
            Address::new([0x21; 32]),
            Address::new([0x22; 32]),
            Address::new([0x23; 32]),
            ALICE,
            None,
        );

        assert!(watcher
            .entombment_needed(Address::new([0x22; 32]))
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn entombment_needed_is_false_once_the_state_exists() {
        let (transport, package, registry, vault) =
            mock_chain(&state(ALICE, None, None), &state(ALICE, None, None), 0);
        let watcher = RotationWatcher::new(transport, package, registry, vault, ALICE, None);

        assert!(!watcher.entombment_needed(registry).await.unwrap());
        assert!(!watcher.entombment_needed(vault).await.unwrap());
    }

    #[tokio::test]
    async fn entombment_needed_propagates_transport_errors() {
        // A non-NotFound read failure must surface as an error: an unreadable
        // chain state must never be read as "needs entombment".
        let (mut transport, package, registry, vault) =
            mock_chain(&state(ALICE, None, None), &state(ALICE, None, None), 0);
        transport.rpc_error_for = Some(rotation_state_field_id(registry, package));
        let watcher = RotationWatcher::new(transport, package, registry, vault, ALICE, None);

        assert!(matches!(
            watcher.entombment_needed(registry).await,
            Err(crate::inbox::InboxError::Rpc(_))
        ));
    }

    // ---------- adopted-key holder ----------

    #[test]
    fn the_adopted_key_holder_starts_empty_and_keeps_the_latest() {
        let holder = AdoptedKey::new();
        assert!(holder.get().is_none());

        let first = Ed25519PrivateKey::new([0x11; 32]);
        let first_addr = first.public_key().derive_address();
        holder.set(first);
        assert_eq!(
            holder.get().map(|k| k.public_key().derive_address()),
            Some(first_addr)
        );

        // A re-adopt (rotation to a third key) overwrites.
        let second = Ed25519PrivateKey::new([0x22; 32]);
        let second_addr = second.public_key().derive_address();
        holder.set(second);
        assert_eq!(
            holder.get().map(|k| k.public_key().derive_address()),
            Some(second_addr)
        );
    }
}
