/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Event-loop-owned network lifecycle facts used by raw pending-state observations.
//!
//! This registry deliberately keeps physical operations separate from their stable pending-source
//! identities. Redirects allocate a new physical identity, while terminal delivery remains visible
//! until the event-loop task which consumes it has actually run.

#![expect(dead_code)]

use std::collections::BTreeMap;

use embedder_traits::document_pending::{
    PendingExternalIoEvidence, PendingExternalIoObservation, PendingExternalIoOwner,
    PendingExternalIoPhase, PendingNetworkKind, PendingNetworkObservation, PendingParserSourceKind,
    PendingSourceDisposition, PendingSourceId,
};
use servo_base::id::{PipelineId, WebViewId};
use timers::DocumentTime;

/// Checked, event-loop-local identity for one physical network operation.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct PendingNetworkOperationId(u64);

impl PendingNetworkOperationId {
    pub(crate) const ZERO: Self = Self(0);

    pub(crate) const fn new(sequence: u64) -> Self {
        Self(sequence)
    }

    pub(crate) const fn get(self) -> u64 {
        self.0
    }

    const fn checked_next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(next) => Some(Self(next)),
            None => None,
        }
    }
}

/// Parser or navigation identity which caused a physical network operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PendingNetworkParent {
    pub(crate) source_id: PendingSourceId,
    pub(crate) kind: PendingParserSourceKind,
}

/// Immutable start-site facts for one physical network operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PendingNetworkStartFacts {
    pub(crate) webview_id: WebViewId,
    pub(crate) pipeline_id: PipelineId,
    pub(crate) kind: PendingNetworkKind,
    pub(crate) evidence: PendingExternalIoEvidence,
    pub(crate) started_at: DocumentTime,
    pub(crate) parent: Option<PendingNetworkParent>,
}

/// Complete event-loop-owned record for one not-yet-handled physical operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PendingNetworkRecord {
    operation_id: PendingNetworkOperationId,
    source_id: PendingSourceId,
    facts: PendingNetworkStartFacts,
    phase: PendingExternalIoPhase,
    redirect_successor: Option<PendingNetworkOperationId>,
}

impl PendingNetworkRecord {
    pub(crate) const fn operation_id(self) -> PendingNetworkOperationId {
        self.operation_id
    }

    pub(crate) const fn source_id(self) -> PendingSourceId {
        self.source_id
    }

    pub(crate) const fn facts(self) -> PendingNetworkStartFacts {
        self.facts
    }

    pub(crate) const fn phase(self) -> PendingExternalIoPhase {
        self.phase
    }

    pub(crate) const fn redirect_successor(self) -> Option<PendingNetworkOperationId> {
        self.redirect_successor
    }

    pub(crate) const fn source_disposition(self) -> PendingSourceDisposition {
        match self.phase {
            PendingExternalIoPhase::TerminalTaskQueued => PendingSourceDisposition::Ready,
            PendingExternalIoPhase::Queued |
            PendingExternalIoPhase::AwaitingResponse |
            PendingExternalIoPhase::StreamingBody => {
                PendingSourceDisposition::AwaitingExternalIo(self.facts.evidence)
            },
        }
    }

    const fn observation(self) -> PendingExternalIoObservation {
        PendingExternalIoObservation {
            source_id: self.source_id,
            pipeline_id: self.facts.pipeline_id,
            kind: self.facts.kind,
            phase: self.phase,
            evidence: self.facts.evidence,
            started_at: self.facts.started_at,
        }
    }
}

/// Sticky physical-registry failure or an invalid lifecycle handoff.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PendingNetworkRegistryError {
    OperationIdExhausted,
    UnknownOperation(PendingNetworkOperationId),
    DuplicateSource(PendingSourceId),
    InvalidPhaseTransition {
        operation_id: PendingNetworkOperationId,
        from: PendingExternalIoPhase,
        to: PendingExternalIoPhase,
    },
    TerminalTaskNotQueued(PendingNetworkOperationId),
    RedirectSourceNotTerminal(PendingNetworkOperationId),
    RedirectSuccessorAlreadyStarted(PendingNetworkOperationId),
    RequiredParentMissing(PendingExternalIoOwner),
    ParentOwnerMismatch {
        owner: PendingExternalIoOwner,
        parent: PendingParserSourceKind,
    },
    SnapshotInvariant,
}

/// Physical network-operation ledger owned by one script event loop.
#[derive(Debug, Default)]
pub(crate) struct PendingNetworkRegistry {
    last_operation_id: PendingNetworkOperationId,
    operation_id_exhausted: bool,
    active: BTreeMap<PendingNetworkOperationId, PendingNetworkRecord>,
    by_source: BTreeMap<PendingSourceId, PendingNetworkOperationId>,
}

impl PendingNetworkRegistry {
    pub(crate) fn validate_start_facts(
        facts: PendingNetworkStartFacts,
    ) -> Result<(), PendingNetworkRegistryError> {
        validate_parent_facts(facts)
    }

    pub(crate) fn start(
        &mut self,
        source_id: PendingSourceId,
        facts: PendingNetworkStartFacts,
    ) -> Result<PendingNetworkOperationId, PendingNetworkRegistryError> {
        Self::validate_start_facts(facts)?;
        if self.by_source.contains_key(&source_id) {
            return Err(PendingNetworkRegistryError::DuplicateSource(source_id));
        }
        let operation_id = self.allocate_operation_id()?;
        let record = PendingNetworkRecord {
            operation_id,
            source_id,
            facts,
            phase: PendingExternalIoPhase::Queued,
            redirect_successor: None,
        };
        let prior = self.active.insert(operation_id, record);
        debug_assert!(prior.is_none());
        let prior = self.by_source.insert(source_id, operation_id);
        debug_assert!(prior.is_none());
        Ok(operation_id)
    }

    /// Start the next physical operation in a redirect chain without consuming the old terminal.
    pub(crate) fn start_redirect(
        &mut self,
        redirected_operation: PendingNetworkOperationId,
        source_id: PendingSourceId,
        started_at: DocumentTime,
    ) -> Result<PendingNetworkOperationId, PendingNetworkRegistryError> {
        let redirected = self.validate_redirect(redirected_operation)?;
        let facts = PendingNetworkStartFacts {
            started_at,
            ..redirected.facts
        };
        let successor = self.start(source_id, facts)?;
        self.active
            .get_mut(&redirected_operation)
            .expect("validated redirect predecessor must remain registered")
            .redirect_successor = Some(successor);
        Ok(successor)
    }

    pub(crate) fn validate_redirect(
        &self,
        redirected_operation: PendingNetworkOperationId,
    ) -> Result<PendingNetworkRecord, PendingNetworkRegistryError> {
        let redirected = self.active.get(&redirected_operation).copied().ok_or(
            PendingNetworkRegistryError::UnknownOperation(redirected_operation),
        )?;
        if redirected.phase != PendingExternalIoPhase::TerminalTaskQueued {
            return Err(PendingNetworkRegistryError::RedirectSourceNotTerminal(
                redirected_operation,
            ));
        }
        if redirected.redirect_successor.is_some() {
            return Err(
                PendingNetworkRegistryError::RedirectSuccessorAlreadyStarted(redirected_operation),
            );
        }
        Ok(redirected)
    }

    pub(crate) fn transition(
        &mut self,
        operation_id: PendingNetworkOperationId,
        phase: PendingExternalIoPhase,
    ) -> Result<PendingNetworkRecord, PendingNetworkRegistryError> {
        self.validate_transition(operation_id, phase)?;
        let record = self
            .active
            .get_mut(&operation_id)
            .ok_or(PendingNetworkRegistryError::UnknownOperation(operation_id))?;
        if record.phase == phase {
            return Ok(*record);
        }
        record.phase = phase;
        Ok(*record)
    }

    pub(crate) fn validate_transition(
        &self,
        operation_id: PendingNetworkOperationId,
        phase: PendingExternalIoPhase,
    ) -> Result<PendingNetworkRecord, PendingNetworkRegistryError> {
        let record = self
            .active
            .get(&operation_id)
            .copied()
            .ok_or(PendingNetworkRegistryError::UnknownOperation(operation_id))?;
        if record.phase != phase && !valid_phase_transition(record.phase, phase) {
            return Err(PendingNetworkRegistryError::InvalidPhaseTransition {
                operation_id,
                from: record.phase,
                to: phase,
            });
        }
        Ok(record)
    }

    pub(crate) fn queue_terminal_task(
        &mut self,
        operation_id: PendingNetworkOperationId,
    ) -> Result<PendingNetworkRecord, PendingNetworkRegistryError> {
        self.transition(operation_id, PendingExternalIoPhase::TerminalTaskQueued)
    }

    /// Remove an operation only after its terminal task has been handled by the event loop.
    pub(crate) fn terminal_task_handled(
        &mut self,
        operation_id: PendingNetworkOperationId,
    ) -> Result<PendingNetworkRecord, PendingNetworkRegistryError> {
        let record = self
            .active
            .get(&operation_id)
            .copied()
            .ok_or(PendingNetworkRegistryError::UnknownOperation(operation_id))?;
        if record.phase != PendingExternalIoPhase::TerminalTaskQueued {
            return Err(PendingNetworkRegistryError::TerminalTaskNotQueued(
                operation_id,
            ));
        }
        self.active.remove(&operation_id);
        self.by_source.remove(&record.source_id);
        Ok(record)
    }

    pub(crate) fn get(
        &self,
        operation_id: PendingNetworkOperationId,
    ) -> Option<PendingNetworkRecord> {
        self.active.get(&operation_id).copied()
    }

    pub(crate) fn snapshot(
        &self,
        webview_id: WebViewId,
    ) -> Result<PendingNetworkObservation, PendingNetworkRegistryError> {
        PendingNetworkObservation::new(
            self.active
                .values()
                .filter(|record| record.facts.webview_id == webview_id)
                .copied()
                .map(PendingNetworkRecord::observation)
                .collect(),
        )
        .map_err(|_| PendingNetworkRegistryError::SnapshotInvariant)
    }

    pub(crate) const fn operation_id_exhausted(&self) -> bool {
        self.operation_id_exhausted
    }

    fn allocate_operation_id(
        &mut self,
    ) -> Result<PendingNetworkOperationId, PendingNetworkRegistryError> {
        if self.operation_id_exhausted {
            return Err(PendingNetworkRegistryError::OperationIdExhausted);
        }
        let Some(next) = self.last_operation_id.checked_next() else {
            self.operation_id_exhausted = true;
            return Err(PendingNetworkRegistryError::OperationIdExhausted);
        };
        self.last_operation_id = next;
        Ok(next)
    }
}

fn validate_parent_facts(
    facts: PendingNetworkStartFacts,
) -> Result<(), PendingNetworkRegistryError> {
    let required_kind = match facts.evidence.owner {
        PendingExternalIoOwner::TopLevelNavigation => {
            Some(PendingParserSourceKind::TopLevelNavigation)
        },
        PendingExternalIoOwner::DocumentParser => Some(PendingParserSourceKind::DocumentParser),
        PendingExternalIoOwner::Script |
        PendingExternalIoOwner::DocumentSubresource |
        PendingExternalIoOwner::RenderingResource |
        PendingExternalIoOwner::Other => None,
    };
    let Some(required_kind) = required_kind else {
        return Ok(());
    };
    let parent = facts
        .parent
        .ok_or(PendingNetworkRegistryError::RequiredParentMissing(
            facts.evidence.owner,
        ))?;
    if parent.kind != required_kind {
        return Err(PendingNetworkRegistryError::ParentOwnerMismatch {
            owner: facts.evidence.owner,
            parent: parent.kind,
        });
    }
    Ok(())
}

const fn valid_phase_transition(from: PendingExternalIoPhase, to: PendingExternalIoPhase) -> bool {
    matches!(
        (from, to),
        (
            PendingExternalIoPhase::Queued,
            PendingExternalIoPhase::AwaitingResponse | PendingExternalIoPhase::TerminalTaskQueued
        ) | (
            PendingExternalIoPhase::AwaitingResponse,
            PendingExternalIoPhase::StreamingBody | PendingExternalIoPhase::TerminalTaskQueued
        ) | (
            PendingExternalIoPhase::StreamingBody,
            PendingExternalIoPhase::TerminalTaskQueued
        )
    )
}

#[cfg(test)]
mod tests {
    use embedder_traits::document_pending::{
        PendingExternalIoLoadBlocking, PendingExternalIoOwner,
    };
    use servo_base::id::{TEST_PIPELINE_ID, TEST_WEBVIEW_ID};

    use super::*;

    fn facts(started_at: u128) -> PendingNetworkStartFacts {
        PendingNetworkStartFacts {
            webview_id: TEST_WEBVIEW_ID,
            pipeline_id: TEST_PIPELINE_ID,
            kind: PendingNetworkKind::Fetch,
            evidence: PendingExternalIoEvidence {
                owner: PendingExternalIoOwner::Script,
                load_blocking: PendingExternalIoLoadBlocking::NonBlocking,
            },
            started_at: DocumentTime::from_nanos(started_at),
            parent: None,
        }
    }

    #[test]
    fn terminal_operation_remains_visible_until_handoff_is_handled() {
        let mut registry = PendingNetworkRegistry::default();
        let operation = registry.start(PendingSourceId::new(1), facts(5)).unwrap();

        registry.queue_terminal_task(operation).unwrap();
        let snapshot = registry.snapshot(TEST_WEBVIEW_ID).unwrap();
        assert_eq!(snapshot.active().len(), 1);
        assert_eq!(
            snapshot.active()[0].phase,
            PendingExternalIoPhase::TerminalTaskQueued
        );
        assert_eq!(
            registry
                .terminal_task_handled(operation)
                .unwrap()
                .source_id(),
            PendingSourceId::new(1)
        );
        assert!(
            registry
                .snapshot(TEST_WEBVIEW_ID)
                .unwrap()
                .active()
                .is_empty()
        );
    }

    #[test]
    fn terminal_handoff_rejects_an_operation_still_waiting_on_io() {
        let mut registry = PendingNetworkRegistry::default();
        let operation = registry.start(PendingSourceId::new(1), facts(5)).unwrap();

        assert_eq!(
            registry.terminal_task_handled(operation),
            Err(PendingNetworkRegistryError::TerminalTaskNotQueued(
                operation
            ))
        );
        assert!(registry.get(operation).is_some());
    }

    #[test]
    fn redirect_allocates_a_new_physical_operation_and_retains_old_terminal() {
        let mut registry = PendingNetworkRegistry::default();
        let first = registry.start(PendingSourceId::new(1), facts(5)).unwrap();
        registry.queue_terminal_task(first).unwrap();

        let second = registry
            .start_redirect(first, PendingSourceId::new(2), DocumentTime::from_nanos(8))
            .unwrap();

        assert_ne!(first, second);
        assert_eq!(
            registry.get(first).unwrap().phase(),
            PendingExternalIoPhase::TerminalTaskQueued
        );
        assert_eq!(
            registry.get(second).unwrap().phase(),
            PendingExternalIoPhase::Queued
        );
        assert_eq!(registry.get(second).unwrap().facts().kind, facts(5).kind);
        assert_eq!(
            registry.get(second).unwrap().facts().started_at,
            DocumentTime::from_nanos(8)
        );
        assert_eq!(
            registry.get(first).unwrap().redirect_successor(),
            Some(second)
        );
        assert_eq!(
            registry.start_redirect(first, PendingSourceId::new(3), DocumentTime::from_nanos(9)),
            Err(PendingNetworkRegistryError::RedirectSuccessorAlreadyStarted(first))
        );
        assert_eq!(
            registry.snapshot(TEST_WEBVIEW_ID).unwrap().active().len(),
            2
        );
        registry.terminal_task_handled(first).unwrap();
        assert_eq!(
            registry.snapshot(TEST_WEBVIEW_ID).unwrap().active().len(),
            1
        );
    }

    #[test]
    fn physical_operation_id_exhaustion_is_sticky() {
        let mut registry = PendingNetworkRegistry {
            last_operation_id: PendingNetworkOperationId::new(u64::MAX),
            ..PendingNetworkRegistry::default()
        };

        assert_eq!(
            registry.start(PendingSourceId::new(1), facts(5)),
            Err(PendingNetworkRegistryError::OperationIdExhausted)
        );
        assert!(registry.operation_id_exhausted());
        assert_eq!(
            registry.start(PendingSourceId::new(2), facts(6)),
            Err(PendingNetworkRegistryError::OperationIdExhausted)
        );
    }

    #[test]
    fn parser_and_navigation_owned_operations_require_parent_identity() {
        let mut registry = PendingNetworkRegistry::default();
        let mut start = facts(5);
        start.evidence.owner = PendingExternalIoOwner::TopLevelNavigation;

        assert_eq!(
            registry.start(PendingSourceId::new(1), start),
            Err(PendingNetworkRegistryError::RequiredParentMissing(
                PendingExternalIoOwner::TopLevelNavigation
            ))
        );
        assert_eq!(registry.last_operation_id, PendingNetworkOperationId::ZERO);
    }
}
