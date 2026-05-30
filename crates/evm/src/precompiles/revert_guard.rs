//! `OpenHlRevertGuard` — REVM `Inspector` that rolls back precompile
//! mutations when the calling EVM frame reverts.
//!
//! ### The bug it closes
//!
//! Stage 17c–17e shipped the deposit/withdraw precompiles and
//! Stage 9c shipped `clob_place_order`. All three mutate
//! process-global state ([`super::ACCOUNTS_STATE`], [`super::CLOB_STATE`],
//! [`super::FILL_SINK`]) directly. If a contract calls the precompile
//! and then `REVERT`s — or hits any other failure mode the EVM
//! rolls back — the precompile's mutation **stays**. The bridge's
//! view drifts from the EVM's view, and a malicious contract can
//! mint collateral by depositing inside a reverted call.
//!
//! ### What this inspector does
//!
//! Pushes a [`BridgeStateSnapshot`] onto a per-EVM stack on every
//! `call` entry. On `call_end`, pops the snapshot; if the call
//! reverted, restores. The behaviour mirrors REVM's own journaling
//! of storage slots — every call frame is a savepoint, every
//! revert rewinds to it.
//!
//! v0 takes whole-state snapshots (clone the entire accounts map +
//! CLOB book + pending-fills buffer per call). For the 5-account
//! dev seed that's noise; for production it should evolve to
//! per-mutation journal entries the way REVM's storage journal
//! works. Out of scope here.
//!
//! ### Scope of Stage 17i
//!
//! This crate provides the **mechanism + tests**. Production wiring
//! (replacing the `NoOpInspector` in
//! [`crate::OpenHlEvmFactory::create_evm`]) is a deliberate
//! follow-up — it touches every block execution in Reth and needs
//! its own integration pass.
//!
//! ### Process-global caveat
//!
//! Like every precompile-globals consumer in this crate, the guard
//! assumes single-EVM-at-a-time execution. Parallel EVMs would
//! trample each other's snapshots — the same caveat that motivates
//! the `#[ignore]` annotations on the Stage 17f end-to-end tests.

use alloy_evm::revm::{
    interpreter::{CallInputs, CallOutcome, InterpreterTypes},
    Inspector,
};

use super::{restore_bridge_state, snapshot_bridge_state, BridgeStateSnapshot};

/// REVM `Inspector` that journals the openhl precompile globals
/// across EVM call frames. See the module docstring for the full
/// rationale.
#[derive(Debug, Default)]
pub struct OpenHlRevertGuard {
    /// One snapshot per active call frame, in entry order. `call`
    /// pushes, `call_end` pops. A `Vec` mirrors REVM's natural call
    /// stack — no need for explicit frame IDs.
    savepoints: Vec<BridgeStateSnapshot>,
}

impl OpenHlRevertGuard {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of frames currently journaled. Test-only inspection
    /// point; in normal operation the stack is empty at transaction
    /// end (every `call` is paired with a `call_end`).
    #[must_use]
    pub fn depth(&self) -> usize {
        self.savepoints.len()
    }
}

impl<CTX, INTR> Inspector<CTX, INTR> for OpenHlRevertGuard
where
    INTR: InterpreterTypes,
{
    fn call(&mut self, _context: &mut CTX, _inputs: &mut CallInputs) -> Option<CallOutcome> {
        // Snapshot at every frame, not just those calling our
        // precompiles. Cheap for v0; sidesteps having to track
        // which frames mutated our state. Reverting from a frame
        // that didn't mutate anything is a no-op restore.
        self.savepoints.push(snapshot_bridge_state());
        // Returning `None` lets the EVM proceed with the call as
        // normal — we only observe, never short-circuit.
        None
    }

    fn call_end(
        &mut self,
        _context: &mut CTX,
        _inputs: &CallInputs,
        outcome: &mut CallOutcome,
    ) {
        let Some(snap) = self.savepoints.pop() else {
            // call_end without a matching call — shouldn't happen
            // with REVM's strict pairing, but stay safe rather than
            // panicking inside the EVM's hot path.
            return;
        };
        // Restore on revert OR halt (out-of-gas, stack overflow,
        // etc.). `is_ok` matches Stop / Return / SelfDestruct —
        // everything else either reverted explicitly or halted,
        // and from the bridge's perspective both must roll back.
        if !outcome.result.result.is_ok() {
            restore_bridge_state(snap);
        }
    }
}

