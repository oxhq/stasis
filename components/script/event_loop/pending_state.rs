/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Owner-led pending-state identities and policy-neutral raw snapshot construction.
//!
//! Mutation APIs in this module are intentionally event-loop-owned. The final builder is pure: it
//! consumes already-copied owner facts, binds sticky terminals to their exact owners, and delegates
//! all cross-inventory validation to [`RawPendingSnapshot::validate`].

#![expect(dead_code)]

use std::collections::BTreeMap;

use embedder_traits::document_pending::{
    DomEpoch, PendingClockObservation, PendingClockTerminal, PendingClockTerminalObservation,
    PendingEventLoopGenerationTerminalObservation, PendingGenerationTerminal,
    PendingGenerationTerminalObservation, PendingInputObservation, PendingInputRevision,
    PendingLogicalTimerKind, PendingLogicalTimerObservation, PendingLogicalTimerSnapshot,
    PendingLogicalTimerStableId, PendingMicrotaskCheckpoint, PendingMicrotaskObservation,
    PendingMicrotaskTerminal, PendingMicrotaskTerminalObservation, PendingNetworkKind,
    PendingNetworkObservation, PendingOpenEndedSourceReason,
    PendingOuterSchedulerTerminalObservation, PendingParserObservation, PendingParserPhase,
    PendingParserSourceKind, PendingParserSourceObservation, PendingProducerObservation,
    PendingProducerPriorEmptyQualification, PendingProducerStability,
    PendingProducerTerminalObservation, PendingRenderingObservation, PendingRuntimeTerminals,
    PendingSchedulerObservation, PendingSnapshotInvariantError, PendingSourceDisposition,
    PendingSourceEpoch, PendingSourceId, PendingSourceIdTerminalObservation, PendingSourceKind,
    PendingSourceObservation, PendingSourceSnapshot, PendingTargetObservation,
    PendingTaskObservation, RawPendingSnapshot, RuntimeStateGeneration,
};
use servo_base::id::{MessagePortId, PipelineId, ScriptEventLoopId, WebViewId};
use timers::{
    DocumentExecutionObservation, DocumentProducerCheckpoint, DocumentProducerFenceId,
    DocumentProducerKind, DocumentProducerObservation, DocumentProducerSnapshot, DocumentTime,
    TimerControlError, TimerDeadlineSnapshot,
};

use super::pending_network::{
    PendingNetworkOperationId, PendingNetworkParent, PendingNetworkRecord, PendingNetworkRegistry,
    PendingNetworkRegistryError, PendingNetworkStartFacts,
};

/// Stable engine identity for one logical DOM timer registration.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct PendingLogicalTimerIdentity {
    pub(crate) pipeline_id: PipelineId,
    pub(crate) stable_id: PendingLogicalTimerStableId,
}

/// Copied policy-neutral facts for one stable logical timer registration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PendingLogicalTimerFacts {
    pub(crate) identity: PendingLogicalTimerIdentity,
    pub(crate) creation_sequence: u64,
    pub(crate) kind: PendingLogicalTimerKind,
    pub(crate) logical_deadline: DocumentTime,
    pub(crate) suspended: bool,
    pub(crate) eligible_in_controlled_turn: bool,
    pub(crate) is_ordering_head: bool,
    pub(crate) delivery_ready: bool,
    pub(crate) outer_wake: Option<TimerDeadlineSnapshot>,
}

impl PendingLogicalTimerFacts {
    const fn source_disposition(self) -> PendingSourceDisposition {
        if self.suspended {
            return PendingSourceDisposition::Inert;
        }
        if self.delivery_ready {
            return PendingSourceDisposition::Ready;
        }
        match self.kind {
            PendingLogicalTimerKind::JavaScriptInterval { requested_period } => {
                PendingSourceDisposition::OpenEnded(PendingOpenEndedSourceReason::Interval {
                    requested_period,
                })
            },
            PendingLogicalTimerKind::EventSourceReconnect => {
                PendingSourceDisposition::OpenEnded(PendingOpenEndedSourceReason::EventSource)
            },
            PendingLogicalTimerKind::JavaScriptOneShot
            | PendingLogicalTimerKind::XmlHttpRequestTimeout
            | PendingLogicalTimerKind::RefreshRedirect
            | PendingLogicalTimerKind::RunStepsAfterTimeout
            | PendingLogicalTimerKind::TestBindingCallback => {
                let deadline = match self.outer_wake {
                    Some(wake) => wake.deadline,
                    None => self.logical_deadline,
                };
                PendingSourceDisposition::FiniteDeadline(deadline)
            },
        }
    }

    const fn observation(self, source_id: PendingSourceId) -> PendingLogicalTimerObservation {
        PendingLogicalTimerObservation {
            source_id,
            pipeline_id: self.identity.pipeline_id,
            stable_id: self.identity.stable_id,
            creation_sequence: self.creation_sequence,
            kind: self.kind,
            logical_deadline: self.logical_deadline,
            suspended: self.suspended,
            eligible_in_controlled_turn: self.eligible_in_controlled_turn,
            is_ordering_head: self.is_ordering_head,
            delivery_ready: self.delivery_ready,
            outer_wake: self.outer_wake,
        }
    }
}

/// Stable engine identity supplied by one live parser or navigation owner.
///
/// Zero is reserved so a default/uninitialized owner identity cannot silently alias live work.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct PendingParserOwnerId(u64);

impl PendingParserOwnerId {
    pub(crate) const fn try_new(value: u64) -> Option<Self> {
        if value == 0 { None } else { Some(Self(value)) }
    }

    pub(crate) const fn get(self) -> u64 {
        self.0
    }
}

/// Copied policy-neutral facts for one stable parser or top-level navigation owner.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PendingParserFacts {
    pub(crate) owner_id: PendingParserOwnerId,
    pub(crate) pipeline_id: PipelineId,
    pub(crate) kind: PendingParserSourceKind,
    pub(crate) phase: PendingParserPhase,
    pub(crate) disposition: PendingSourceDisposition,
}

/// Stable ledger identity for one live parser or top-level navigation owner.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct PendingParserIdentity {
    pub(crate) source_id: PendingSourceId,
    pub(crate) owner_id: PendingParserOwnerId,
    pub(crate) pipeline_id: PipelineId,
    pub(crate) kind: PendingParserSourceKind,
}

impl PendingParserIdentity {
    const fn stable_key(self) -> PendingParserStableKey {
        PendingParserStableKey {
            owner_id: self.owner_id,
            pipeline_id: self.pipeline_id,
            kind: self.kind,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct PendingParserStableKey {
    owner_id: PendingParserOwnerId,
    pipeline_id: PipelineId,
    kind: PendingParserSourceKind,
}

impl From<PendingParserFacts> for PendingParserStableKey {
    fn from(facts: PendingParserFacts) -> Self {
        Self {
            owner_id: facts.owner_id,
            pipeline_id: facts.pipeline_id,
            kind: facts.kind,
        }
    }
}

/// Stable native identity for a retained source which can produce future externally triggered
/// work. Variants are namespaced so unrelated native identity domains can never alias.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum PendingPersistentSourceStableId {
    WebSocket(u64),
    EventSource(u64),
    BroadcastChannel(u128),
    MessagePort(MessagePortId),
    MediaSessionActionHandler,
    StorageEventListener,
    Worker,
}

/// Complete owner identity for one retained persistent source.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct PendingPersistentSourceIdentity {
    pub(crate) pipeline_id: PipelineId,
    pub(crate) stable_id: PendingPersistentSourceStableId,
}

impl PendingPersistentSourceIdentity {
    const fn source_kind(self) -> PendingSourceKind {
        match self.stable_id {
            PendingPersistentSourceStableId::WebSocket(_)
            | PendingPersistentSourceStableId::EventSource(_) => PendingSourceKind::Network,
            PendingPersistentSourceStableId::BroadcastChannel(_)
            | PendingPersistentSourceStableId::MessagePort(_)
            | PendingPersistentSourceStableId::MediaSessionActionHandler
            | PendingPersistentSourceStableId::StorageEventListener
            | PendingPersistentSourceStableId::Worker => PendingSourceKind::TrackedPresence,
        }
    }

    const fn source_disposition(self) -> PendingSourceDisposition {
        let reason = match self.stable_id {
            PendingPersistentSourceStableId::WebSocket(_) => {
                PendingOpenEndedSourceReason::WebSocket
            },
            PendingPersistentSourceStableId::EventSource(_) => {
                PendingOpenEndedSourceReason::EventSource
            },
            PendingPersistentSourceStableId::BroadcastChannel(_) => {
                PendingOpenEndedSourceReason::BroadcastChannel
            },
            PendingPersistentSourceStableId::MessagePort(_) => {
                PendingOpenEndedSourceReason::MessagePort
            },
            PendingPersistentSourceStableId::MediaSessionActionHandler => {
                PendingOpenEndedSourceReason::MediaSessionActionHandler
            },
            PendingPersistentSourceStableId::StorageEventListener => {
                PendingOpenEndedSourceReason::StorageEventListener
            },
            PendingPersistentSourceStableId::Worker => {
                return PendingSourceDisposition::Unsupported(
                    embedder_traits::document_pending::PendingUnsupportedSourceReason::Worker,
                );
            },
        };
        PendingSourceDisposition::OpenEnded(reason)
    }

    const fn is_valid(self) -> bool {
        match self.stable_id {
            PendingPersistentSourceStableId::WebSocket(id)
            | PendingPersistentSourceStableId::EventSource(id) => id != 0,
            PendingPersistentSourceStableId::BroadcastChannel(id) => id != 0,
            PendingPersistentSourceStableId::MessagePort(_)
            | PendingPersistentSourceStableId::MediaSessionActionHandler
            | PendingPersistentSourceStableId::StorageEventListener
            | PendingPersistentSourceStableId::Worker => true,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum PendingStableSourceKey {
    LogicalTimer(PendingLogicalTimerIdentity),
    Parser(PendingParserStableKey),
    Persistent(PendingPersistentSourceIdentity),
    Network(PendingNetworkOperationId),
    ProducerResourceFallback(u64),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PendingResourceFallback {
    fence_id: DocumentProducerFenceId,
    source_id: PendingSourceId,
    pipeline_id: PipelineId,
    started_at: DocumentTime,
}

impl PendingResourceFallback {
    const EVIDENCE: embedder_traits::document_pending::PendingExternalIoEvidence =
        embedder_traits::document_pending::PendingExternalIoEvidence {
            owner: embedder_traits::document_pending::PendingExternalIoOwner::Other,
            load_blocking:
                embedder_traits::document_pending::PendingExternalIoLoadBlocking::Unknown,
        };

    const fn observation(self) -> embedder_traits::document_pending::PendingExternalIoObservation {
        embedder_traits::document_pending::PendingExternalIoObservation {
            source_id: self.source_id,
            pipeline_id: self.pipeline_id,
            kind: PendingNetworkKind::ProducerFallback,
            phase: embedder_traits::document_pending::PendingExternalIoPhase::AwaitingResponse,
            evidence: Self::EVIDENCE,
            started_at: self.started_at,
        }
    }
}

#[derive(Clone, Debug)]
struct PendingWebViewState {
    state_generation: RuntimeStateGeneration,
    state_generation_terminal: Option<PendingGenerationTerminalObservation>,
    source_epoch: PendingSourceEpoch,
    source_epoch_terminal: Option<PendingGenerationTerminalObservation>,
    logical_timers_authoritative: bool,
    parser_authoritative: bool,
    persistent_sources_authoritative: bool,
    resource_fence_authority: Option<DocumentProducerFenceId>,
    source_keys: BTreeMap<PendingStableSourceKey, PendingSourceId>,
    sources: BTreeMap<PendingSourceId, PendingSourceObservation>,
    logical_timers: BTreeMap<PendingSourceId, PendingLogicalTimerObservation>,
    parsers: BTreeMap<PendingSourceId, PendingParserSourceObservation>,
    resource_fallback: Option<PendingResourceFallback>,
    last_normalized_snapshot: Option<Box<RawPendingSnapshot>>,
}

impl PendingWebViewState {
    fn new() -> Self {
        Self {
            state_generation: RuntimeStateGeneration::ZERO,
            state_generation_terminal: None,
            source_epoch: PendingSourceEpoch::ZERO,
            source_epoch_terminal: None,
            logical_timers_authoritative: false,
            parser_authoritative: false,
            persistent_sources_authoritative: false,
            resource_fence_authority: None,
            source_keys: BTreeMap::new(),
            sources: BTreeMap::new(),
            logical_timers: BTreeMap::new(),
            parsers: BTreeMap::new(),
            resource_fallback: None,
            last_normalized_snapshot: None,
        }
    }
}

/// Source registration returned for one physical network operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PendingNetworkRegistration {
    pub(crate) operation_id: PendingNetworkOperationId,
    pub(crate) source_id: PendingSourceId,
}

/// Checked owner-ledger failure. Variants are typed so integration never maps string messages.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PendingStateError {
    DuplicateWebView(WebViewId),
    UnknownWebView(WebViewId),
    UnknownSource(PendingSourceId),
    UnknownParser(PendingParserIdentity),
    DuplicateParserOwner(PendingParserOwnerId),
    DuplicatePersistentSource(PendingPersistentSourceIdentity),
    InvalidPersistentSource(PendingPersistentSourceIdentity),
    ResourceFenceAlreadyBound {
        expected: DocumentProducerFenceId,
        observed: DocumentProducerFenceId,
    },
    StateGenerationExhausted(WebViewId),
    SourceEpochExhausted(WebViewId),
    SourceIdExhausted,
    NetworkOperationIdExhausted,
    Network(PendingNetworkRegistryError),
    MissingNetworkParent(PendingNetworkParent),
    NetworkParentKindMismatch {
        parent: PendingNetworkParent,
        observed: PendingParserSourceKind,
    },
    NetworkParentPipelineMismatch {
        parent: PendingNetworkParent,
        parent_pipeline: PipelineId,
        operation_pipeline: PipelineId,
    },
    ProducerPriorEmptyMissing,
    SnapshotInvariant(PendingSnapshotInvariantError),
}

/// Failure while binding a pure raw build to the event-loop owner's checked generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PendingNormalizeError {
    State(PendingStateError),
    Build(PendingBuildError),
    StaleOwnerFacts,
    ResourceFallbackTargetUnavailable(WebViewId),
    ResourceFenceAuthorityMismatch {
        expected: DocumentProducerFenceId,
        observed: DocumentProducerFenceId,
    },
    PersistentSourceOutsideTarget(PendingPersistentSourceIdentity),
    NonMonotonicStateGeneration {
        previous: RuntimeStateGeneration,
        observed: RuntimeStateGeneration,
    },
}

impl From<PendingStateError> for PendingNormalizeError {
    fn from(error: PendingStateError) -> Self {
        Self::State(error)
    }
}

impl From<PendingBuildError> for PendingNormalizeError {
    fn from(error: PendingBuildError) -> Self {
        Self::Build(error)
    }
}

impl From<PendingNetworkRegistryError> for PendingStateError {
    fn from(error: PendingNetworkRegistryError) -> Self {
        Self::Network(error)
    }
}

impl From<PendingSnapshotInvariantError> for PendingStateError {
    fn from(error: PendingSnapshotInvariantError) -> Self {
        Self::SnapshotInvariant(error)
    }
}

/// Complete owner facts copied without retaining a borrow into the event-loop ledger.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PendingOwnerSnapshotFacts {
    pub(crate) event_loop_id: ScriptEventLoopId,
    pub(crate) webview_id: WebViewId,
    pub(crate) state_generation: RuntimeStateGeneration,
    pub(crate) sources: Option<PendingSourceSnapshot>,
    pub(crate) logical_timers: Option<PendingLogicalTimerSnapshot>,
    pub(crate) parser: Option<PendingParserObservation>,
    pub(crate) network: Option<PendingNetworkObservation>,
    pub(crate) state_generation_terminal: Option<PendingGenerationTerminalObservation>,
    pub(crate) source_epoch_terminal: Option<PendingGenerationTerminalObservation>,
    pub(crate) source_id_terminal: Option<PendingSourceIdTerminalObservation>,
}

/// Per-event-loop identity and authoritative-source ledger.
#[derive(Debug)]
pub(crate) struct PendingStateLedger {
    event_loop_id: ScriptEventLoopId,
    last_source_id: PendingSourceId,
    source_id_terminal: Option<PendingSourceIdTerminalObservation>,
    webviews: BTreeMap<WebViewId, PendingWebViewState>,
    network: PendingNetworkRegistry,
}

impl PendingStateLedger {
    pub(crate) fn new(event_loop_id: ScriptEventLoopId) -> Self {
        Self {
            event_loop_id,
            last_source_id: PendingSourceId::ZERO,
            source_id_terminal: None,
            webviews: BTreeMap::new(),
            network: PendingNetworkRegistry::default(),
        }
    }

    pub(crate) fn register_webview(
        &mut self,
        webview_id: WebViewId,
    ) -> Result<(), PendingStateError> {
        if self.webviews.contains_key(&webview_id) {
            return Err(PendingStateError::DuplicateWebView(webview_id));
        }
        self.webviews.insert(webview_id, PendingWebViewState::new());
        Ok(())
    }

    /// Atomically replace one WebView's complete logical-timer inventory.
    ///
    /// This is the authoritative capture API. It prevalidates owner-wide head/coalescing facts,
    /// preserves source IDs for matching stable registrations, removes disappeared timers, and
    /// only marks the inventory authoritative when the complete candidate can be committed.
    pub(crate) fn replace_logical_timers(
        &mut self,
        webview_id: WebViewId,
        mut timers: Vec<PendingLogicalTimerFacts>,
    ) -> Result<(), PendingStateError> {
        timers.sort_unstable_by_key(|timer| timer.identity);
        let provisional = timers
            .iter()
            .enumerate()
            .map(|(index, timer)| {
                let source_id = u64::try_from(index)
                    .ok()
                    .and_then(|index| index.checked_add(1))
                    .map(PendingSourceId::new)
                    .ok_or(PendingStateError::SourceIdExhausted)?;
                Ok(timer.observation(source_id))
            })
            .collect::<Result<Vec<_>, PendingStateError>>()?;
        PendingLogicalTimerSnapshot::new(provisional)?;

        let original = self.webview(webview_id)?.clone();
        let mut candidate = original.clone();
        let desired = timers
            .iter()
            .map(|timer| (PendingStableSourceKey::LogicalTimer(timer.identity), *timer))
            .collect::<BTreeMap<_, _>>();
        let new_source_count = u64::try_from(
            desired
                .keys()
                .filter(|key| !original.source_keys.contains_key(key))
                .count(),
        )
        .map_err(|_| PendingStateError::SourceIdExhausted)?;
        if new_source_count != 0 {
            if self.source_id_terminal.is_some() {
                return Err(PendingStateError::SourceIdExhausted);
            }
            let remaining = u64::MAX - self.last_source_id.get();
            if new_source_count > remaining {
                if remaining == 0 {
                    self.source_id_terminal = Some(PendingSourceIdTerminalObservation {
                        event_loop_id: self.event_loop_id,
                        last_issued: self.last_source_id,
                        error: PendingGenerationTerminal::Exhausted,
                    });
                }
                return Err(PendingStateError::SourceIdExhausted);
            }
        }

        let existing = original
            .source_keys
            .iter()
            .filter_map(|(key, source_id)| match key {
                PendingStableSourceKey::LogicalTimer(_) => Some((*key, *source_id)),
                _ => None,
            })
            .collect::<Vec<_>>();
        for (key, source_id) in existing {
            if desired.contains_key(&key) {
                continue;
            }
            candidate.source_keys.remove(&key);
            candidate.sources.remove(&source_id);
            candidate.logical_timers.remove(&source_id);
        }

        let mut last_source_id = self.last_source_id;
        for (key, timer) in desired {
            let source_id = if let Some(source_id) = original.source_keys.get(&key).copied() {
                source_id
            } else {
                let next = last_source_id.checked_next().expect(
                    "batch source capacity was checked before logical-timer reconciliation",
                );
                last_source_id = next;
                next
            };
            candidate.source_keys.insert(key, source_id);
            candidate.sources.insert(
                source_id,
                PendingSourceObservation {
                    id: source_id,
                    kind: PendingSourceKind::Timer,
                    disposition: timer.source_disposition(),
                },
            );
            candidate
                .logical_timers
                .insert(source_id, timer.observation(source_id));
        }
        PendingLogicalTimerSnapshot::new(candidate.logical_timers.values().copied().collect())?;

        let sources_changed =
            candidate.sources != original.sources || candidate.source_keys != original.source_keys;
        let timers_changed = candidate.logical_timers != original.logical_timers;
        let authority_changed = !original.logical_timers_authoritative;
        if !sources_changed && !timers_changed && !authority_changed {
            return Ok(());
        }

        if sources_changed {
            let (next_state, next_epoch) = self.prepare_source_advance(webview_id)?;
            candidate.state_generation = next_state;
            candidate.source_epoch = next_epoch;
        } else {
            candidate.state_generation = self.prepare_state_advance(webview_id)?;
        }
        let terminals = self.webview(webview_id)?;
        candidate.state_generation_terminal = terminals.state_generation_terminal;
        candidate.source_epoch_terminal = terminals.source_epoch_terminal;
        candidate.logical_timers_authoritative = true;
        self.last_source_id = last_source_id;
        self.webviews.insert(webview_id, candidate);
        Ok(())
    }

    /// Atomically replace one WebView's complete parser/navigation-owner inventory.
    ///
    /// Native owner identity participates in the stable key. Replacing a parser in place on the
    /// same pipeline therefore cannot reuse the removed parser's public source identity.
    pub(crate) fn replace_parsers(
        &mut self,
        webview_id: WebViewId,
        mut parsers: Vec<PendingParserFacts>,
    ) -> Result<(), PendingStateError> {
        parsers.sort_unstable_by_key(|parser| (parser.pipeline_id, parser.kind, parser.owner_id));
        for duplicate in parsers.windows(2) {
            let left = PendingParserStableKey::from(duplicate[0]);
            let right = PendingParserStableKey::from(duplicate[1]);
            if left == right {
                return Err(PendingStateError::DuplicateParserOwner(left.owner_id));
            }
        }
        let provisional = parsers
            .iter()
            .enumerate()
            .map(|(index, parser)| {
                let source_id = u64::try_from(index)
                    .ok()
                    .and_then(|index| index.checked_add(1))
                    .map(PendingSourceId::new)
                    .ok_or(PendingStateError::SourceIdExhausted)?;
                Ok(PendingParserSourceObservation {
                    source_id,
                    pipeline_id: parser.pipeline_id,
                    kind: parser.kind,
                    phase: parser.phase,
                    disposition: parser.disposition,
                })
            })
            .collect::<Result<Vec<_>, PendingStateError>>()?;
        PendingParserObservation::new(provisional)?;

        let original = self.webview(webview_id)?.clone();
        let mut candidate = original.clone();
        let desired = parsers
            .into_iter()
            .map(|parser| (PendingStableSourceKey::Parser(parser.into()), parser))
            .collect::<BTreeMap<_, _>>();
        let new_source_count = u64::try_from(
            desired
                .keys()
                .filter(|key| !original.source_keys.contains_key(key))
                .count(),
        )
        .map_err(|_| PendingStateError::SourceIdExhausted)?;
        self.preflight_source_capacity(new_source_count)?;

        let existing = original
            .source_keys
            .iter()
            .filter_map(|(key, source_id)| match key {
                PendingStableSourceKey::Parser(_) => Some((*key, *source_id)),
                _ => None,
            })
            .collect::<Vec<_>>();
        for (key, source_id) in existing {
            if desired.contains_key(&key) {
                continue;
            }
            candidate.source_keys.remove(&key);
            candidate.sources.remove(&source_id);
            candidate.parsers.remove(&source_id);
        }

        let mut last_source_id = self.last_source_id;
        for (key, parser) in desired {
            let source_id = if let Some(source_id) = original.source_keys.get(&key).copied() {
                source_id
            } else {
                let next = last_source_id
                    .checked_next()
                    .expect("batch source capacity was checked before parser reconciliation");
                last_source_id = next;
                next
            };
            candidate.source_keys.insert(key, source_id);
            candidate.sources.insert(
                source_id,
                PendingSourceObservation {
                    id: source_id,
                    kind: PendingSourceKind::Parser,
                    disposition: parser.disposition,
                },
            );
            candidate.parsers.insert(
                source_id,
                PendingParserSourceObservation {
                    source_id,
                    pipeline_id: parser.pipeline_id,
                    kind: parser.kind,
                    phase: parser.phase,
                    disposition: parser.disposition,
                },
            );
        }
        PendingParserObservation::new(candidate.parsers.values().copied().collect())?;

        let sources_changed =
            candidate.sources != original.sources || candidate.source_keys != original.source_keys;
        let parsers_changed = candidate.parsers != original.parsers;
        let authority_changed = !original.parser_authoritative;
        if !sources_changed && !parsers_changed && !authority_changed {
            return Ok(());
        }
        if sources_changed {
            let (next_state, next_epoch) = self.prepare_source_advance(webview_id)?;
            candidate.state_generation = next_state;
            candidate.source_epoch = next_epoch;
        } else {
            candidate.state_generation = self.prepare_state_advance(webview_id)?;
        }
        let terminals = self.webview(webview_id)?;
        candidate.state_generation_terminal = terminals.state_generation_terminal;
        candidate.source_epoch_terminal = terminals.source_epoch_terminal;
        candidate.parser_authoritative = true;
        self.last_source_id = last_source_id;
        self.webviews.insert(webview_id, candidate);
        Ok(())
    }

    /// Atomically replace all retained externally-triggered source registrations for one WebView.
    pub(crate) fn replace_persistent_sources(
        &mut self,
        webview_id: WebViewId,
        mut sources: Vec<PendingPersistentSourceIdentity>,
    ) -> Result<(), PendingStateError> {
        sources.sort_unstable();
        for source in &sources {
            if !source.is_valid() {
                return Err(PendingStateError::InvalidPersistentSource(*source));
            }
        }
        for duplicate in sources.windows(2) {
            if duplicate[0] == duplicate[1] {
                return Err(PendingStateError::DuplicatePersistentSource(duplicate[0]));
            }
        }

        let original = self.webview(webview_id)?.clone();
        let mut candidate = original.clone();
        let desired = sources
            .into_iter()
            .map(|identity| (PendingStableSourceKey::Persistent(identity), identity))
            .collect::<BTreeMap<_, _>>();
        let new_source_count = u64::try_from(
            desired
                .keys()
                .filter(|key| !original.source_keys.contains_key(key))
                .count(),
        )
        .map_err(|_| PendingStateError::SourceIdExhausted)?;
        self.preflight_source_capacity(new_source_count)?;

        let existing = original
            .source_keys
            .iter()
            .filter_map(|(key, source_id)| match key {
                PendingStableSourceKey::Persistent(_) => Some((*key, *source_id)),
                _ => None,
            })
            .collect::<Vec<_>>();
        for (key, source_id) in existing {
            if desired.contains_key(&key) {
                continue;
            }
            candidate.source_keys.remove(&key);
            candidate.sources.remove(&source_id);
        }

        let mut last_source_id = self.last_source_id;
        for (key, identity) in desired {
            let source_id = if let Some(source_id) = original.source_keys.get(&key).copied() {
                source_id
            } else {
                let next = last_source_id.checked_next().expect(
                    "batch source capacity was checked before persistent-source reconciliation",
                );
                last_source_id = next;
                next
            };
            candidate.source_keys.insert(key, source_id);
            candidate.sources.insert(
                source_id,
                PendingSourceObservation {
                    id: source_id,
                    kind: identity.source_kind(),
                    disposition: identity.source_disposition(),
                },
            );
        }
        PendingSourceSnapshot::new(
            candidate.source_epoch,
            candidate.sources.values().copied().collect(),
        )?;

        let sources_changed =
            candidate.sources != original.sources || candidate.source_keys != original.source_keys;
        let authority_changed = !original.persistent_sources_authoritative;
        if !sources_changed && !authority_changed {
            return Ok(());
        }
        if sources_changed {
            let (next_state, next_epoch) = self.prepare_source_advance(webview_id)?;
            candidate.state_generation = next_state;
            candidate.source_epoch = next_epoch;
        } else {
            candidate.state_generation = self.prepare_state_advance(webview_id)?;
        }
        let terminals = self.webview(webview_id)?;
        candidate.state_generation_terminal = terminals.state_generation_terminal;
        candidate.source_epoch_terminal = terminals.source_epoch_terminal;
        candidate.persistent_sources_authoritative = true;
        self.last_source_id = last_source_id;
        self.webviews.insert(webview_id, candidate);
        Ok(())
    }

    /// Bind conservative Resource-producer fallback coverage to the exact event-loop fence which
    /// production fetch callbacks use. Normalization rejects any observation from another fence.
    pub(crate) fn bind_resource_fence_network_authority(
        &mut self,
        webview_id: WebViewId,
        fence_id: DocumentProducerFenceId,
    ) -> Result<(), PendingStateError> {
        self.webview(webview_id)?;
        if let Some(expected) = self
            .webviews
            .values()
            .find_map(|state| state.resource_fence_authority)
        {
            if expected != fence_id {
                return Err(PendingStateError::ResourceFenceAlreadyBound {
                    expected,
                    observed: fence_id,
                });
            }
            if self.webview(webview_id)?.resource_fence_authority == Some(expected) {
                return Ok(());
            }
        }
        let next = self.prepare_state_advance(webview_id)?;
        let state = self.webview_mut(webview_id)?;
        state.resource_fence_authority = Some(fence_id);
        state.state_generation = next;
        Ok(())
    }

    /// Register a newly created parser/navigation owner and return its stable source identity.
    pub(crate) fn start_parser(
        &mut self,
        webview_id: WebViewId,
        pipeline_id: PipelineId,
        kind: PendingParserSourceKind,
        phase: PendingParserPhase,
        disposition: PendingSourceDisposition,
    ) -> Result<PendingParserIdentity, PendingStateError> {
        // Validate phase/disposition before allocating an identity or advancing generations.
        let provisional = PendingParserSourceObservation {
            source_id: PendingSourceId::new(1),
            pipeline_id,
            kind,
            phase,
            disposition,
        };
        PendingParserObservation::new(vec![provisional])?;
        let (next_state, next_epoch) = self.prepare_source_advance(webview_id)?;
        let source_id = self.allocate_source_id()?;
        let owner_id = PendingParserOwnerId(source_id.get());
        let identity = PendingParserIdentity {
            source_id,
            owner_id,
            pipeline_id,
            kind,
        };
        let state = self.webview_mut(webview_id)?;
        state.source_keys.insert(
            PendingStableSourceKey::Parser(identity.stable_key()),
            source_id,
        );
        state.sources.insert(
            source_id,
            PendingSourceObservation {
                id: source_id,
                kind: PendingSourceKind::Parser,
                disposition,
            },
        );
        state.parsers.insert(
            source_id,
            PendingParserSourceObservation {
                source_id,
                pipeline_id,
                kind,
                phase,
                disposition,
            },
        );
        state.state_generation = next_state;
        state.source_epoch = next_epoch;
        Ok(identity)
    }

    pub(crate) fn update_parser(
        &mut self,
        webview_id: WebViewId,
        identity: PendingParserIdentity,
        phase: PendingParserPhase,
        disposition: PendingSourceDisposition,
    ) -> Result<(), PendingStateError> {
        if self
            .webview(webview_id)?
            .source_keys
            .get(&PendingStableSourceKey::Parser(identity.stable_key()))
            .copied()
            != Some(identity.source_id)
        {
            return Err(PendingStateError::UnknownParser(identity));
        }
        let current_parser = *self
            .webview(webview_id)?
            .parsers
            .get(&identity.source_id)
            .ok_or(PendingStateError::UnknownParser(identity))?;
        if current_parser.pipeline_id != identity.pipeline_id
            || current_parser.kind != identity.kind
        {
            return Err(PendingStateError::UnknownParser(identity));
        }
        let candidate = PendingParserSourceObservation {
            source_id: identity.source_id,
            pipeline_id: identity.pipeline_id,
            kind: identity.kind,
            phase,
            disposition,
        };
        PendingParserObservation::new(vec![candidate])?;
        if current_parser == candidate {
            return Ok(());
        }
        if current_parser.disposition != disposition {
            let (next_state, next_epoch) = self.prepare_source_advance(webview_id)?;
            let state = self.webview_mut(webview_id)?;
            state.sources.insert(
                identity.source_id,
                PendingSourceObservation {
                    id: identity.source_id,
                    kind: PendingSourceKind::Parser,
                    disposition,
                },
            );
            state.parsers.insert(identity.source_id, candidate);
            state.state_generation = next_state;
            state.source_epoch = next_epoch;
        } else {
            let next_state = self.prepare_state_advance(webview_id)?;
            let state = self.webview_mut(webview_id)?;
            state.parsers.insert(identity.source_id, candidate);
            state.state_generation = next_state;
        }
        Ok(())
    }

    pub(crate) fn remove_parser(
        &mut self,
        webview_id: WebViewId,
        identity: PendingParserIdentity,
    ) -> Result<PendingSourceId, PendingStateError> {
        let key = PendingStableSourceKey::Parser(identity.stable_key());
        let source_id = self
            .webview(webview_id)?
            .source_keys
            .get(&key)
            .copied()
            .ok_or(PendingStateError::UnknownParser(identity))?;
        if source_id != identity.source_id {
            return Err(PendingStateError::UnknownParser(identity));
        }
        let removed = self.remove_source(webview_id, key)?;
        debug_assert_eq!(removed, source_id);
        self.webview_mut(webview_id)?.parsers.remove(&source_id);
        Ok(source_id)
    }

    pub(crate) fn start_network(
        &mut self,
        facts: PendingNetworkStartFacts,
    ) -> Result<PendingNetworkRegistration, PendingStateError> {
        self.validate_network_parent(facts)?;
        let (next_state, next_epoch) = self.prepare_source_advance(facts.webview_id)?;
        let source_id = self.allocate_source_id()?;
        let operation_id = self.network.start(source_id, facts)?;
        let record = self
            .network
            .get(operation_id)
            .expect("a just-started network operation must remain registered");
        self.commit_new_network_source(facts.webview_id, record, next_state, next_epoch)?;
        Ok(PendingNetworkRegistration {
            operation_id,
            source_id,
        })
    }

    pub(crate) fn start_network_redirect(
        &mut self,
        redirected_operation: PendingNetworkOperationId,
        started_at: DocumentTime,
    ) -> Result<PendingNetworkRegistration, PendingStateError> {
        // Derive immutable chain facts from the predecessor, whose parser/navigation parent was
        // validated at initial registration and may legitimately have completed by now.
        let redirected = self.network.validate_redirect(redirected_operation)?;
        let webview_id = redirected.facts().webview_id;
        let (next_state, next_epoch) = self.prepare_source_advance(webview_id)?;
        let source_id = self.allocate_source_id()?;
        let operation_id =
            self.network
                .start_redirect(redirected_operation, source_id, started_at)?;
        let record = self
            .network
            .get(operation_id)
            .expect("a just-started redirect operation must remain registered");
        self.commit_new_network_source(webview_id, record, next_state, next_epoch)?;
        Ok(PendingNetworkRegistration {
            operation_id,
            source_id,
        })
    }

    pub(crate) fn transition_network(
        &mut self,
        operation_id: PendingNetworkOperationId,
        phase: embedder_traits::document_pending::PendingExternalIoPhase,
    ) -> Result<(), PendingStateError> {
        let current = self.network.validate_transition(operation_id, phase)?;
        if current.phase() == phase {
            return Ok(());
        }
        let webview_id = current.facts().webview_id;
        let source_changed = current.source_disposition()
            != disposition_for_network_phase(phase, current.facts().evidence);
        let source_versions = if source_changed {
            Some(self.prepare_source_advance(webview_id)?)
        } else {
            None
        };
        let next_state = if source_changed {
            source_versions.expect("source versions were prepared").0
        } else {
            self.prepare_state_advance(webview_id)?
        };
        let updated = self.network.transition(operation_id, phase)?;
        let state = self.webview_mut(webview_id)?;
        if source_changed {
            let (_, next_epoch) = source_versions.expect("source versions were prepared");
            state.sources.insert(
                updated.source_id(),
                PendingSourceObservation {
                    id: updated.source_id(),
                    kind: PendingSourceKind::Network,
                    disposition: updated.source_disposition(),
                },
            );
            state.source_epoch = next_epoch;
        }
        state.state_generation = next_state;
        Ok(())
    }

    pub(crate) fn queue_network_terminal_task(
        &mut self,
        operation_id: PendingNetworkOperationId,
    ) -> Result<(), PendingStateError> {
        self.transition_network(
            operation_id,
            embedder_traits::document_pending::PendingExternalIoPhase::TerminalTaskQueued,
        )
    }

    pub(crate) fn network_terminal_task_handled(
        &mut self,
        operation_id: PendingNetworkOperationId,
    ) -> Result<PendingSourceId, PendingStateError> {
        let record = self
            .network
            .get(operation_id)
            .ok_or(PendingNetworkRegistryError::UnknownOperation(operation_id))?;
        if record.phase()
            != embedder_traits::document_pending::PendingExternalIoPhase::TerminalTaskQueued
        {
            return Err(PendingNetworkRegistryError::TerminalTaskNotQueued(operation_id).into());
        }
        let webview_id = record.facts().webview_id;
        let (next_state, next_epoch) = self.prepare_source_advance(webview_id)?;
        let removed = self.network.terminal_task_handled(operation_id)?;
        let key = PendingStableSourceKey::Network(operation_id);
        let state = self.webview_mut(webview_id)?;
        state.source_keys.remove(&key);
        state.sources.remove(&removed.source_id());
        state.state_generation = next_state;
        state.source_epoch = next_epoch;
        Ok(removed.source_id())
    }

    pub(crate) fn owner_snapshot(
        &self,
        webview_id: WebViewId,
    ) -> Result<PendingOwnerSnapshotFacts, PendingStateError> {
        let state = self.webview(webview_id)?;
        if self.network.operation_id_exhausted() {
            return Err(PendingStateError::NetworkOperationIdExhausted);
        }
        let sources_complete = state.logical_timers_authoritative
            && state.parser_authoritative
            && state.persistent_sources_authoritative
            && state.resource_fence_authority.is_some();
        let sources = if sources_complete {
            Some(PendingSourceSnapshot::new(
                state.source_epoch,
                state.sources.values().copied().collect(),
            )?)
        } else {
            None
        };
        let logical_timers = if state.logical_timers_authoritative {
            Some(PendingLogicalTimerSnapshot::new(
                state.logical_timers.values().copied().collect(),
            )?)
        } else {
            None
        };
        let parser = if state.parser_authoritative {
            Some(PendingParserObservation::new(
                state.parsers.values().copied().collect(),
            )?)
        } else {
            None
        };
        let network = if state.resource_fence_authority.is_some() {
            let detailed = self.network.snapshot(webview_id)?;
            let mut active = detailed.active().to_vec();
            if let Some(fallback) = state.resource_fallback {
                active.push(fallback.observation());
            }
            Some(PendingNetworkObservation::new(active)?)
        } else {
            None
        };
        Ok(PendingOwnerSnapshotFacts {
            event_loop_id: self.event_loop_id,
            webview_id,
            state_generation: state.state_generation,
            sources,
            logical_timers,
            parser,
            network,
            state_generation_terminal: state.state_generation_terminal,
            source_epoch_terminal: state.source_epoch_terminal,
            source_id_terminal: self.source_id_terminal,
        })
    }

    /// Build and generation-bind a complete raw observation at the owner boundary.
    ///
    /// The pure builder remains usable in isolation, but controlled admission must use this path.
    /// It compares every normalized raw fact except the generation itself. If non-ledger facts
    /// changed while the ledger generation stayed fixed, it advances the checked per-WebView
    /// generation before publishing. Callers therefore cannot accidentally omit a separate
    /// `note_runtime_state_change` for clock, input, microtask, DOM, rendering, or terminal facts.
    pub(crate) fn normalize_and_build(
        &mut self,
        mut facts: RawPendingBuildFacts,
    ) -> Result<RawPendingSnapshot, PendingNormalizeError> {
        let webview_id = facts.target.webview_id;
        let current_owner = self.owner_snapshot(webview_id)?;
        if facts.owner != current_owner {
            return Err(PendingNormalizeError::StaleOwnerFacts);
        }
        if let Some(expected) = self.webview(webview_id)?.resource_fence_authority {
            let observed = facts.producers.fence_id;
            if observed != expected {
                return Err(PendingNormalizeError::ResourceFenceAuthorityMismatch {
                    expected,
                    observed,
                });
            }
        }
        if let Some(identity) = self
            .webview(webview_id)?
            .source_keys
            .keys()
            .find_map(|key| match key {
                PendingStableSourceKey::Persistent(identity)
                    if !facts.target.contains_pipeline(identity.pipeline_id) =>
                {
                    Some(*identity)
                },
                _ => None,
            })
        {
            return Err(PendingNormalizeError::PersistentSourceOutsideTarget(
                identity,
            ));
        }
        // Validate every independently captured fact before the owner ledger allocates, removes,
        // or rehomes a fallback source. Strip only that owner-controlled pair from the preflight:
        // target membership may already have replaced its old pipeline, which is precisely what
        // reconciliation below must repair transactionally.
        let mut preflight = facts.clone();
        if let Some(fallback) = self.webview(webview_id)?.resource_fallback {
            if let Some(sources) = &preflight.owner.sources {
                preflight.owner.sources = Some(
                    PendingSourceSnapshot::new(
                        sources.epoch(),
                        sources
                            .sources()
                            .iter()
                            .copied()
                            .filter(|source| source.id != fallback.source_id)
                            .collect(),
                    )
                    .map_err(PendingBuildError::from)?,
                );
            }
            if let Some(network) = &preflight.owner.network {
                preflight.owner.network = Some(
                    PendingNetworkObservation::new(
                        network
                            .active()
                            .iter()
                            .copied()
                            .filter(|operation| operation.source_id != fallback.source_id)
                            .collect(),
                    )
                    .map_err(PendingBuildError::from)?,
                );
            }
        }
        RawPendingBuilder::build_without_resource_coverage_check(preflight)?;
        self.reconcile_resource_fallback(
            &facts.target,
            facts.clock.observation.now,
            facts.producers.snapshot,
        )?;
        facts.owner = self.owner_snapshot(webview_id)?;
        let mut snapshot = RawPendingBuilder::build(facts)?;

        let (previous_generation, normalized_changed) = {
            let state = self.webview(webview_id)?;
            let previous_generation = state
                .last_normalized_snapshot
                .as_ref()
                .map(|previous| previous.state_generation);
            let normalized_changed = state
                .last_normalized_snapshot
                .as_ref()
                .is_none_or(|previous| !raw_normalized_state_eq(previous, &snapshot));
            (previous_generation, normalized_changed)
        };

        let generation_already_changed = match previous_generation {
            Some(previous) if snapshot.state_generation < previous => {
                return Err(PendingNormalizeError::NonMonotonicStateGeneration {
                    previous,
                    observed: snapshot.state_generation,
                });
            },
            Some(previous) => snapshot.state_generation != previous,
            None => snapshot.state_generation != RuntimeStateGeneration::ZERO,
        };
        if normalized_changed && !generation_already_changed {
            let next = self.prepare_state_advance(webview_id)?;
            let state = self.webview_mut(webview_id)?;
            state.state_generation = next;
            snapshot.state_generation = next;
            snapshot.terminals.state_generation = state.state_generation_terminal;
            snapshot.validate().map_err(PendingBuildError::from)?;
        }

        self.webview_mut(webview_id)?.last_normalized_snapshot = Some(Box::new(snapshot.clone()));
        Ok(snapshot)
    }

    fn reconcile_resource_fallback(
        &mut self,
        target: &PendingTargetObservation,
        now: DocumentTime,
        producers: DocumentProducerSnapshot,
    ) -> Result<(), PendingNormalizeError> {
        let webview_id = target.webview_id;
        let pending_resources = producers.for_kind(DocumentProducerKind::Resource).pending();
        // Physical network records do not yet carry a producer-lease identity. Do not let an
        // unrelated navigation, redirect predecessor, or queued terminal stand in for a Resource
        // ticket: retain one conservative, fence-scoped unknown-I/O source until coverage can be
        // joined explicitly.
        let fallback_needed = pending_resources != 0;
        let current = self.webview(webview_id)?.resource_fallback;
        if fallback_needed
            && current.is_some_and(|fallback| {
                fallback.fence_id == producers.fence_id()
                    && target.contains_pipeline(fallback.pipeline_id)
            })
        {
            return Ok(());
        }
        if !fallback_needed && current.is_none() {
            return Ok(());
        }

        if !fallback_needed {
            let current = current.expect("a resource fallback was observed above");
            let (next_state, next_epoch) = self.prepare_source_advance(webview_id)?;
            let state = self.webview_mut(webview_id)?;
            state
                .source_keys
                .remove(&PendingStableSourceKey::ProducerResourceFallback(
                    current.fence_id.get(),
                ));
            state.sources.remove(&current.source_id);
            state.resource_fallback = None;
            state.state_generation = next_state;
            state.source_epoch = next_epoch;
            return Ok(());
        }

        let pipeline_id = target
            .active_top_level
            .map(|active| active.pipeline_id)
            .or_else(|| target.pending_top_level_pipelines().first().copied())
            .ok_or(PendingNormalizeError::ResourceFallbackTargetUnavailable(
                webview_id,
            ))?;
        if let Some(mut fallback) = current
            && fallback.fence_id == producers.fence_id()
        {
            let next_state = self.prepare_state_advance(webview_id)?;
            fallback.pipeline_id = pipeline_id;
            let state = self.webview_mut(webview_id)?;
            state.resource_fallback = Some(fallback);
            state.state_generation = next_state;
            return Ok(());
        }
        let (next_state, next_epoch) = self.prepare_source_advance(webview_id)?;
        let source_id = self.allocate_source_id()?;
        let fallback = PendingResourceFallback {
            fence_id: producers.fence_id(),
            source_id,
            pipeline_id,
            started_at: now,
        };
        let state = self.webview_mut(webview_id)?;
        if let Some(previous) = current {
            state
                .source_keys
                .remove(&PendingStableSourceKey::ProducerResourceFallback(
                    previous.fence_id.get(),
                ));
            state.sources.remove(&previous.source_id);
        }
        state.source_keys.insert(
            PendingStableSourceKey::ProducerResourceFallback(producers.fence_id().get()),
            source_id,
        );
        state.sources.insert(
            source_id,
            PendingSourceObservation {
                id: source_id,
                kind: PendingSourceKind::Network,
                disposition: PendingSourceDisposition::AwaitingExternalIo(
                    PendingResourceFallback::EVIDENCE,
                ),
            },
        );
        state.resource_fallback = Some(fallback);
        state.state_generation = next_state;
        state.source_epoch = next_epoch;
        Ok(())
    }

    fn upsert_source(
        &mut self,
        webview_id: WebViewId,
        key: PendingStableSourceKey,
        kind: PendingSourceKind,
        disposition: PendingSourceDisposition,
    ) -> Result<PendingSourceId, PendingStateError> {
        let existing = self.webview(webview_id)?.source_keys.get(&key).copied();
        if let Some(source_id) = existing {
            let current = *self
                .webview(webview_id)?
                .sources
                .get(&source_id)
                .ok_or(PendingStateError::UnknownSource(source_id))?;
            let candidate = PendingSourceObservation {
                id: source_id,
                kind,
                disposition,
            };
            if current == candidate {
                return Ok(source_id);
            }
            let (next_state, next_epoch) = self.prepare_source_advance(webview_id)?;
            let state = self.webview_mut(webview_id)?;
            state.sources.insert(source_id, candidate);
            state.state_generation = next_state;
            state.source_epoch = next_epoch;
            return Ok(source_id);
        }
        self.insert_source(webview_id, key, kind, disposition)
    }

    fn insert_source(
        &mut self,
        webview_id: WebViewId,
        key: PendingStableSourceKey,
        kind: PendingSourceKind,
        disposition: PendingSourceDisposition,
    ) -> Result<PendingSourceId, PendingStateError> {
        let (next_state, next_epoch) = self.prepare_source_advance(webview_id)?;
        let source_id = self.allocate_source_id()?;
        let state = self.webview_mut(webview_id)?;
        state.source_keys.insert(key, source_id);
        state.sources.insert(
            source_id,
            PendingSourceObservation {
                id: source_id,
                kind,
                disposition,
            },
        );
        state.state_generation = next_state;
        state.source_epoch = next_epoch;
        Ok(source_id)
    }

    fn remove_source(
        &mut self,
        webview_id: WebViewId,
        key: PendingStableSourceKey,
    ) -> Result<PendingSourceId, PendingStateError> {
        let source_id = self
            .webview(webview_id)?
            .source_keys
            .get(&key)
            .copied()
            .ok_or(PendingStateError::UnknownSource(PendingSourceId::ZERO))?;
        let (next_state, next_epoch) = self.prepare_source_advance(webview_id)?;
        let state = self.webview_mut(webview_id)?;
        state.source_keys.remove(&key);
        state.sources.remove(&source_id);
        state.state_generation = next_state;
        state.source_epoch = next_epoch;
        Ok(source_id)
    }

    fn commit_new_network_source(
        &mut self,
        webview_id: WebViewId,
        record: PendingNetworkRecord,
        next_state: RuntimeStateGeneration,
        next_epoch: PendingSourceEpoch,
    ) -> Result<(), PendingStateError> {
        let state = self.webview_mut(webview_id)?;
        state.source_keys.insert(
            PendingStableSourceKey::Network(record.operation_id()),
            record.source_id(),
        );
        state.sources.insert(
            record.source_id(),
            PendingSourceObservation {
                id: record.source_id(),
                kind: PendingSourceKind::Network,
                disposition: record.source_disposition(),
            },
        );
        state.state_generation = next_state;
        state.source_epoch = next_epoch;
        Ok(())
    }

    fn validate_network_parent(
        &self,
        facts: PendingNetworkStartFacts,
    ) -> Result<(), PendingStateError> {
        PendingNetworkRegistry::validate_start_facts(facts)?;
        let Some(parent) = facts.parent else {
            return Ok(());
        };
        let parser = self
            .webview(facts.webview_id)?
            .parsers
            .get(&parent.source_id)
            .copied()
            .ok_or(PendingStateError::MissingNetworkParent(parent))?;
        if parser.kind != parent.kind {
            return Err(PendingStateError::NetworkParentKindMismatch {
                parent,
                observed: parser.kind,
            });
        }
        if parser.pipeline_id != facts.pipeline_id {
            return Err(PendingStateError::NetworkParentPipelineMismatch {
                parent,
                parent_pipeline: parser.pipeline_id,
                operation_pipeline: facts.pipeline_id,
            });
        }
        Ok(())
    }

    fn allocate_source_id(&mut self) -> Result<PendingSourceId, PendingStateError> {
        if self.source_id_terminal.is_some() {
            return Err(PendingStateError::SourceIdExhausted);
        }
        let Some(next) = self.last_source_id.checked_next() else {
            self.source_id_terminal = Some(PendingSourceIdTerminalObservation {
                event_loop_id: self.event_loop_id,
                last_issued: self.last_source_id,
                error: PendingGenerationTerminal::Exhausted,
            });
            return Err(PendingStateError::SourceIdExhausted);
        };
        self.last_source_id = next;
        Ok(next)
    }

    fn preflight_source_capacity(
        &mut self,
        new_source_count: u64,
    ) -> Result<(), PendingStateError> {
        if new_source_count == 0 {
            return Ok(());
        }
        if self.source_id_terminal.is_some() {
            return Err(PendingStateError::SourceIdExhausted);
        }
        let remaining = u64::MAX - self.last_source_id.get();
        if new_source_count <= remaining {
            return Ok(());
        }
        if remaining == 0 {
            self.source_id_terminal = Some(PendingSourceIdTerminalObservation {
                event_loop_id: self.event_loop_id,
                last_issued: self.last_source_id,
                error: PendingGenerationTerminal::Exhausted,
            });
        }
        Err(PendingStateError::SourceIdExhausted)
    }

    fn prepare_state_advance(
        &mut self,
        webview_id: WebViewId,
    ) -> Result<RuntimeStateGeneration, PendingStateError> {
        let state = self.webview_mut(webview_id)?;
        if state.state_generation_terminal.is_some() {
            return Err(PendingStateError::StateGenerationExhausted(webview_id));
        }
        let Some(next) = state.state_generation.checked_next() else {
            state.state_generation_terminal = Some(PendingGenerationTerminalObservation {
                webview_id,
                error: PendingGenerationTerminal::Exhausted,
            });
            return Err(PendingStateError::StateGenerationExhausted(webview_id));
        };
        Ok(next)
    }

    fn prepare_source_advance(
        &mut self,
        webview_id: WebViewId,
    ) -> Result<(RuntimeStateGeneration, PendingSourceEpoch), PendingStateError> {
        let next_state = self.prepare_state_advance(webview_id)?;
        let state = self.webview_mut(webview_id)?;
        if state.source_epoch_terminal.is_some() {
            return Err(PendingStateError::SourceEpochExhausted(webview_id));
        }
        let Some(next_epoch) = state.source_epoch.checked_next() else {
            state.source_epoch_terminal = Some(PendingGenerationTerminalObservation {
                webview_id,
                error: PendingGenerationTerminal::Exhausted,
            });
            return Err(PendingStateError::SourceEpochExhausted(webview_id));
        };
        Ok((next_state, next_epoch))
    }

    fn webview(&self, webview_id: WebViewId) -> Result<&PendingWebViewState, PendingStateError> {
        self.webviews
            .get(&webview_id)
            .ok_or(PendingStateError::UnknownWebView(webview_id))
    }

    fn webview_mut(
        &mut self,
        webview_id: WebViewId,
    ) -> Result<&mut PendingWebViewState, PendingStateError> {
        self.webviews
            .get_mut(&webview_id)
            .ok_or(PendingStateError::UnknownWebView(webview_id))
    }
}

const fn disposition_for_network_phase(
    phase: embedder_traits::document_pending::PendingExternalIoPhase,
    evidence: embedder_traits::document_pending::PendingExternalIoEvidence,
) -> PendingSourceDisposition {
    match phase {
        embedder_traits::document_pending::PendingExternalIoPhase::TerminalTaskQueued => {
            PendingSourceDisposition::Ready
        },
        embedder_traits::document_pending::PendingExternalIoPhase::Queued
        | embedder_traits::document_pending::PendingExternalIoPhase::AwaitingResponse
        | embedder_traits::document_pending::PendingExternalIoPhase::StreamingBody => {
            PendingSourceDisposition::AwaitingExternalIo(evidence)
        },
    }
}

/// Event-loop state which turns producer-observer results into checkpoint-qualified raw evidence.
#[derive(Debug)]
pub(crate) struct PendingProducerQualificationLedger {
    event_loop_id: ScriptEventLoopId,
    last_empty: Option<PendingProducerPriorEmptyQualification>,
    last_qualified: Option<PendingProducerObservation>,
}

impl PendingProducerQualificationLedger {
    pub(crate) const fn new(event_loop_id: ScriptEventLoopId) -> Self {
        Self {
            event_loop_id,
            last_empty: None,
            last_qualified: None,
        }
    }

    pub(crate) fn not_checkpointed(
        &mut self,
        snapshot: DocumentProducerSnapshot,
    ) -> Result<PendingProducerObservation, PendingStateError> {
        self.last_empty = None;
        self.last_qualified = None;
        PendingProducerObservation::new(
            self.event_loop_id,
            PendingMicrotaskCheckpoint::ZERO,
            DocumentProducerCheckpoint::ZERO,
            snapshot,
            PendingProducerStability::NotCheckpointed,
            None,
        )
        .map_err(Into::into)
    }

    /// Reuse checkpoint-qualified evidence while both owner checkpoints and the exact fence
    /// snapshot remain unchanged.
    ///
    /// A passive control observation does not create a new qualification boundary. It also must
    /// not erase an existing one: doing so makes an otherwise unchanged quiescent snapshot
    /// advance its public state generation merely because it was observed. Any checkpoint or
    /// producer-fence change still fails closed as `Unqualified`.
    pub(crate) fn passive(
        &mut self,
        microtask_checkpoint: PendingMicrotaskCheckpoint,
        checkpoint: DocumentProducerCheckpoint,
        fresh_snapshot: DocumentProducerSnapshot,
    ) -> Result<PendingProducerObservation, PendingStateError> {
        if let Some(qualified) = self.last_qualified
            && qualified.microtask_checkpoint == microtask_checkpoint
            && qualified.checkpoint == checkpoint
            && qualified.snapshot == fresh_snapshot
        {
            return Ok(qualified);
        }

        self.last_qualified = None;
        PendingProducerObservation::new(
            self.event_loop_id,
            microtask_checkpoint,
            checkpoint,
            fresh_snapshot,
            PendingProducerStability::Unqualified,
            None,
        )
        .map_err(Into::into)
    }

    /// Bind a qualified checkpoint observation to a fresh post-observation fence snapshot.
    ///
    /// A producer can begin and finish between the observer checkpoint and this capture. Even if
    /// the fresh snapshot is empty again, its changed revision makes the checkpoint evidence
    /// unqualified rather than stable.
    pub(crate) fn qualify(
        &mut self,
        microtask_checkpoint: PendingMicrotaskCheckpoint,
        checkpoint: DocumentProducerCheckpoint,
        observation: DocumentProducerObservation,
        fresh_snapshot: DocumentProducerSnapshot,
    ) -> Result<PendingProducerObservation, PendingStateError> {
        let observed_snapshot = producer_observation_snapshot(observation);
        if observed_snapshot != fresh_snapshot {
            self.last_empty = None;
            self.last_qualified = None;
            return PendingProducerObservation::new(
                self.event_loop_id,
                microtask_checkpoint,
                checkpoint,
                fresh_snapshot,
                PendingProducerStability::Unqualified,
                None,
            )
            .map_err(Into::into);
        }

        match observation {
            DocumentProducerObservation::Busy(_) => {
                self.last_empty = None;
                let pending = PendingProducerObservation::from_observation(
                    self.event_loop_id,
                    microtask_checkpoint,
                    checkpoint,
                    observation,
                    None,
                )
                .map_err(PendingStateError::from)?;
                self.last_qualified = Some(pending);
                Ok(pending)
            },
            DocumentProducerObservation::FirstEmpty(snapshot) => {
                let pending = PendingProducerObservation::from_observation(
                    self.event_loop_id,
                    microtask_checkpoint,
                    checkpoint,
                    observation,
                    None,
                )?;
                self.last_empty = Some(PendingProducerPriorEmptyQualification {
                    microtask_checkpoint,
                    checkpoint,
                    snapshot_revision: snapshot.revision(),
                });
                self.last_qualified = Some(pending);
                Ok(pending)
            },
            DocumentProducerObservation::StableEmpty(snapshot) => {
                let prior = self
                    .last_empty
                    .ok_or(PendingStateError::ProducerPriorEmptyMissing)?;
                let pending = PendingProducerObservation::from_observation(
                    self.event_loop_id,
                    microtask_checkpoint,
                    checkpoint,
                    observation,
                    Some(prior),
                )?;
                self.last_empty = Some(PendingProducerPriorEmptyQualification {
                    microtask_checkpoint,
                    checkpoint,
                    snapshot_revision: snapshot.revision(),
                });
                self.last_qualified = Some(pending);
                Ok(pending)
            },
        }
    }
}

const fn producer_observation_snapshot(
    observation: DocumentProducerObservation,
) -> DocumentProducerSnapshot {
    match observation {
        DocumentProducerObservation::Busy(snapshot)
        | DocumentProducerObservation::FirstEmpty(snapshot)
        | DocumentProducerObservation::StableEmpty(snapshot) => snapshot,
    }
}

/// Copied ordinary-task counts from the event-loop queue owner.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct PendingTaskFacts {
    pub(crate) ready: usize,
    pub(crate) throttled: usize,
    pub(crate) inactive: usize,
}

/// Copied ordinary-input facts from the controlled event-loop barrier.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct PendingInputFacts {
    pub(crate) revision: PendingInputRevision,
    pub(crate) revision_exhausted: bool,
    pub(crate) ready_events: usize,
    pub(crate) intake_saturated: bool,
    pub(crate) tasks: PendingTaskFacts,
}

/// Copied microtask-owner facts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PendingMicrotaskFacts {
    pub(crate) queued: usize,
    pub(crate) completed_checkpoint: PendingMicrotaskCheckpoint,
    pub(crate) checkpoint_in_progress: bool,
    pub(crate) terminal: Option<PendingMicrotaskTerminal>,
}

/// Copied document-clock facts with the sticky terminal retained by that clock.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PendingClockFacts {
    pub(crate) observation: PendingClockObservation,
    pub(crate) terminal: Option<PendingClockTerminal>,
}

/// Copied outer-scheduler facts with the sticky terminal retained by that scheduler.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PendingSchedulerFacts {
    pub(crate) observation: PendingSchedulerObservation,
    pub(crate) terminal: Option<TimerControlError>,
}

/// Every fact required by the pure raw-pending builder.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RawPendingBuildFacts {
    pub(crate) target: PendingTargetObservation,
    pub(crate) owner: PendingOwnerSnapshotFacts,
    pub(crate) dom_epoch: DomEpoch,
    pub(crate) clock: PendingClockFacts,
    pub(crate) scheduler: PendingSchedulerFacts,
    pub(crate) input: PendingInputFacts,
    pub(crate) microtasks: PendingMicrotaskFacts,
    pub(crate) execution: Option<DocumentExecutionObservation>,
    pub(crate) producers: PendingProducerObservation,
    pub(crate) rendering: Option<PendingRenderingObservation>,
    /// Terminals from owners outside this ledger: target routing, DOM, logical/image timers, and
    /// navigation or pipeline-membership counters.
    pub(crate) supplemental_terminals: PendingRuntimeTerminals,
}

/// Required authoritative inventory missing from a raw snapshot capture.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PendingFactKind {
    Sources,
    LogicalTimers,
    Parser,
    Network,
    Rendering,
}

/// Builder-owned terminal slot which a caller attempted to splice through supplemental facts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PendingOwnedTerminalSlot {
    Clock,
    OuterScheduler,
    Producer,
    Microtask,
    InputRevision,
    SourceId,
    StateGeneration,
    SourceEpoch,
}

/// Count whose in-memory owner representation could not fit the raw wire representation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PendingCountKind {
    ReadyEvents,
    ReadyTasks,
    ThrottledTasks,
    InactiveTasks,
    Microtasks,
}

/// Typed failure from pure raw snapshot construction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PendingBuildError {
    MissingFact(PendingFactKind),
    WebViewOwnerMismatch {
        target: WebViewId,
        owner: WebViewId,
    },
    EventLoopOwnerMismatch {
        target: ScriptEventLoopId,
        owner: ScriptEventLoopId,
    },
    OwnedTerminalConflict(PendingOwnedTerminalSlot),
    CountOverflow(PendingCountKind),
    InputRevisionTerminalBeforeExhaustion,
    UnrepresentedResourceProducer,
    Invariant(PendingSnapshotInvariantError),
}

impl From<PendingSnapshotInvariantError> for PendingBuildError {
    fn from(error: PendingSnapshotInvariantError) -> Self {
        Self::Invariant(error)
    }
}

/// Pure constructor for one complete, policy-neutral pending-state observation.
pub(crate) struct RawPendingBuilder;

impl RawPendingBuilder {
    pub(crate) fn build(
        facts: RawPendingBuildFacts,
    ) -> Result<RawPendingSnapshot, PendingBuildError> {
        Self::build_internal(facts, true)
    }

    fn build_without_resource_coverage_check(
        facts: RawPendingBuildFacts,
    ) -> Result<RawPendingSnapshot, PendingBuildError> {
        Self::build_internal(facts, false)
    }

    fn build_internal(
        facts: RawPendingBuildFacts,
        require_resource_coverage: bool,
    ) -> Result<RawPendingSnapshot, PendingBuildError> {
        let RawPendingBuildFacts {
            target,
            owner,
            dom_epoch,
            clock,
            scheduler,
            input,
            microtasks,
            execution,
            producers,
            rendering,
            mut supplemental_terminals,
        } = facts;
        if owner.webview_id != target.webview_id {
            return Err(PendingBuildError::WebViewOwnerMismatch {
                target: target.webview_id,
                owner: owner.webview_id,
            });
        }
        if owner.event_loop_id != target.event_loop_id {
            return Err(PendingBuildError::EventLoopOwnerMismatch {
                target: target.event_loop_id,
                owner: owner.event_loop_id,
            });
        }
        reject_owned_terminal_conflicts(&supplemental_terminals)?;

        let sources = owner
            .sources
            .ok_or(PendingBuildError::MissingFact(PendingFactKind::Sources))?;
        let logical_timers = owner.logical_timers.ok_or(PendingBuildError::MissingFact(
            PendingFactKind::LogicalTimers,
        ))?;
        let parser = owner
            .parser
            .ok_or(PendingBuildError::MissingFact(PendingFactKind::Parser))?;
        let network = owner
            .network
            .ok_or(PendingBuildError::MissingFact(PendingFactKind::Network))?;
        let rendering =
            rendering.ok_or(PendingBuildError::MissingFact(PendingFactKind::Rendering))?;
        if require_resource_coverage
            && producers
                .snapshot
                .for_kind(DocumentProducerKind::Resource)
                .pending()
                != 0
            && !network.active().iter().any(|operation| {
                operation.kind == PendingNetworkKind::ProducerFallback
                    && operation.evidence == PendingResourceFallback::EVIDENCE
            })
        {
            return Err(PendingBuildError::UnrepresentedResourceProducer);
        }

        supplemental_terminals.clock =
            clock.terminal.map(|error| PendingClockTerminalObservation {
                clock_id: clock.observation.clock_id,
                error,
            });
        supplemental_terminals.outer_scheduler =
            scheduler
                .terminal
                .map(|error| PendingOuterSchedulerTerminalObservation {
                    event_loop_id: target.event_loop_id,
                    scheduler_id: scheduler.observation.scheduler_id,
                    error,
                });
        supplemental_terminals.producer =
            producers
                .snapshot
                .terminal_error()
                .map(|error| PendingProducerTerminalObservation {
                    fence_id: producers.fence_id,
                    error,
                });
        supplemental_terminals.microtask =
            microtasks
                .terminal
                .map(|error| PendingMicrotaskTerminalObservation {
                    event_loop_id: target.event_loop_id,
                    error,
                });
        if input.revision_exhausted {
            if input.revision.get() != u64::MAX {
                return Err(PendingBuildError::InputRevisionTerminalBeforeExhaustion);
            }
            supplemental_terminals.input_revision =
                Some(PendingEventLoopGenerationTerminalObservation {
                    event_loop_id: target.event_loop_id,
                    error: PendingGenerationTerminal::Exhausted,
                });
        }
        supplemental_terminals.source_id = owner.source_id_terminal;
        supplemental_terminals.state_generation = owner.state_generation_terminal;
        supplemental_terminals.source_epoch = owner.source_epoch_terminal;

        let snapshot = RawPendingSnapshot {
            target,
            state_generation: owner.state_generation,
            dom_epoch,
            clock: clock.observation,
            scheduler: scheduler.observation,
            input: PendingInputObservation {
                revision: input.revision,
                ready_events: checked_count(input.ready_events, PendingCountKind::ReadyEvents)?,
                intake_saturated: input.intake_saturated,
                tasks: PendingTaskObservation {
                    ready: checked_count(input.tasks.ready, PendingCountKind::ReadyTasks)?,
                    throttled: checked_count(
                        input.tasks.throttled,
                        PendingCountKind::ThrottledTasks,
                    )?,
                    inactive: checked_count(input.tasks.inactive, PendingCountKind::InactiveTasks)?,
                },
            },
            microtasks: PendingMicrotaskObservation {
                event_loop_id: owner.event_loop_id,
                queued: checked_count(microtasks.queued, PendingCountKind::Microtasks)?,
                completed_checkpoint: microtasks.completed_checkpoint,
                checkpoint_in_progress: microtasks.checkpoint_in_progress,
                terminal: microtasks.terminal,
            },
            execution,
            producers,
            parser,
            network,
            logical_timers,
            rendering,
            sources,
            terminals: supplemental_terminals,
        };
        snapshot.validate()?;
        Ok(snapshot)
    }
}

fn reject_owned_terminal_conflicts(
    terminals: &PendingRuntimeTerminals,
) -> Result<(), PendingBuildError> {
    for (present, slot) in [
        (terminals.clock.is_some(), PendingOwnedTerminalSlot::Clock),
        (
            terminals.outer_scheduler.is_some(),
            PendingOwnedTerminalSlot::OuterScheduler,
        ),
        (
            terminals.producer.is_some(),
            PendingOwnedTerminalSlot::Producer,
        ),
        (
            terminals.microtask.is_some(),
            PendingOwnedTerminalSlot::Microtask,
        ),
        (
            terminals.input_revision.is_some(),
            PendingOwnedTerminalSlot::InputRevision,
        ),
        (
            terminals.source_id.is_some(),
            PendingOwnedTerminalSlot::SourceId,
        ),
        (
            terminals.state_generation.is_some(),
            PendingOwnedTerminalSlot::StateGeneration,
        ),
        (
            terminals.source_epoch.is_some(),
            PendingOwnedTerminalSlot::SourceEpoch,
        ),
    ] {
        if present {
            return Err(PendingBuildError::OwnedTerminalConflict(slot));
        }
    }
    Ok(())
}

fn checked_count(value: usize, kind: PendingCountKind) -> Result<u64, PendingBuildError> {
    u64::try_from(value).map_err(|_| PendingBuildError::CountOverflow(kind))
}

fn raw_normalized_state_eq(left: &RawPendingSnapshot, right: &RawPendingSnapshot) -> bool {
    let mut left = left.clone();
    let mut right = right.clone();
    left.state_generation = RuntimeStateGeneration::ZERO;
    right.state_generation = RuntimeStateGeneration::ZERO;
    // Exhaustion is checked before guarded control. It cannot consume a new generation once the
    // counter is already at MAX, so exclude only this terminal's self-referential owner slot.
    left.terminals.state_generation = None;
    right.terminals.state_generation = None;
    left == right
}

#[cfg(test)]
mod tests {
    use embedder_traits::document_pending::{
        PendingActiveTopLevelPipeline, PendingAnimatedImageObservation, PendingCanvasObservation,
        PendingClockMode, PendingExternalIoLoadBlocking, PendingExternalIoOwner,
        PendingNavigationRevision, PendingPipelineMembershipRevision,
        PendingPipelineRenderingObservation, PendingRenderingPipelineActivity,
    };
    use servo_base::Epoch;
    use servo_base::id::{
        BrowsingContextId, BrowsingContextIndex, Index, PipelineIndex, PipelineNamespaceId,
        TEST_PIPELINE_ID, TEST_SCRIPT_EVENT_LOOP_ID, TEST_WEBVIEW_ID,
    };
    use timers::{
        DocumentClock, DocumentClockConfiguration, DocumentExecutionCounters,
        DocumentExecutionLimits, DocumentProducerFence, DocumentProducerObserver, DocumentUnixTime,
        TimerEventRequest, TimerScheduler,
    };

    use super::*;

    fn ledger() -> PendingStateLedger {
        let mut ledger = PendingStateLedger::new(TEST_SCRIPT_EVENT_LOOP_ID);
        ledger.register_webview(TEST_WEBVIEW_ID).unwrap();
        ledger
    }

    fn complete_ledger(producers: PendingProducerObservation) -> PendingStateLedger {
        let mut ledger = ledger();
        ledger
            .replace_logical_timers(TEST_WEBVIEW_ID, Vec::new())
            .unwrap();
        ledger.replace_parsers(TEST_WEBVIEW_ID, Vec::new()).unwrap();
        ledger
            .replace_persistent_sources(TEST_WEBVIEW_ID, Vec::new())
            .unwrap();
        ledger
            .bind_resource_fence_network_authority(TEST_WEBVIEW_ID, producers.fence_id)
            .unwrap();
        ledger
    }

    fn timer_identity() -> PendingLogicalTimerIdentity {
        PendingLogicalTimerIdentity {
            pipeline_id: TEST_PIPELINE_ID,
            stable_id: PendingLogicalTimerStableId::JavaScriptHandle(7),
        }
    }

    fn timer_facts(creation_sequence: u64) -> PendingLogicalTimerFacts {
        PendingLogicalTimerFacts {
            identity: timer_identity(),
            creation_sequence,
            kind: PendingLogicalTimerKind::JavaScriptOneShot,
            logical_deadline: DocumentTime::from_nanos(10),
            suspended: true,
            eligible_in_controlled_turn: false,
            is_ordering_head: false,
            delivery_ready: false,
            outer_wake: None,
        }
    }

    fn active_timer_facts(
        stable_id: PendingLogicalTimerStableId,
        creation_sequence: u64,
        kind: PendingLogicalTimerKind,
    ) -> PendingLogicalTimerFacts {
        let clock = DocumentClock::new(DocumentClockConfiguration::Controlled {
            initial_time_ns: 0,
            unix_time_origin_ns: DocumentUnixTime::default(),
        });
        let mut scheduler = TimerScheduler::with_clock(clock);
        scheduler
            .try_schedule_timer(TimerEventRequest {
                callback: Box::new(|| {}),
                duration: std::time::Duration::from_nanos(10),
            })
            .unwrap();
        let outer_wake = scheduler.finite_deadline_snapshot().unwrap().unwrap();
        PendingLogicalTimerFacts {
            identity: PendingLogicalTimerIdentity {
                pipeline_id: TEST_PIPELINE_ID,
                stable_id,
            },
            creation_sequence,
            kind,
            logical_deadline: outer_wake.deadline,
            suspended: false,
            eligible_in_controlled_turn: true,
            is_ordering_head: true,
            delivery_ready: false,
            outer_wake: Some(outer_wake),
        }
    }

    fn target() -> PendingTargetObservation {
        target_for(TEST_PIPELINE_ID)
    }

    fn target_for(pipeline_id: PipelineId) -> PendingTargetObservation {
        PendingTargetObservation::new_with_authority(
            TEST_WEBVIEW_ID,
            TEST_SCRIPT_EVENT_LOOP_ID,
            Some(PendingActiveTopLevelPipeline {
                pipeline_id,
                epoch: Epoch(1),
            }),
            PendingNavigationRevision::ZERO,
            PendingPipelineMembershipRevision::ZERO,
            None,
            vec![pipeline_id],
            vec![pipeline_id],
            Vec::new(),
        )
        .unwrap()
    }

    fn rendering() -> PendingRenderingObservation {
        rendering_for(TEST_PIPELINE_ID)
    }

    fn rendering_for(pipeline_id: PipelineId) -> PendingRenderingObservation {
        PendingRenderingObservation::new(
            None,
            false,
            vec![PendingPipelineRenderingObservation {
                pipeline_id,
                activity: PendingRenderingPipelineActivity::FullyActive,
                retained_animation_frame_callbacks: 0,
                runnable_animation_frame_callbacks: 0,
                document_update_required: false,
                pending_animation_events: 0,
                finite_animations: 0,
                infinite_animations: 0,
                unsupported_animations: 0,
                animated_images: PendingAnimatedImageObservation::default(),
                canvas: PendingCanvasObservation::default(),
                pending_fonts: 0,
                pending_images: 0,
            }],
        )
        .unwrap()
    }

    fn empty_producers() -> PendingProducerObservation {
        let fence = DocumentProducerFence::default();
        let checkpoint = DocumentProducerCheckpoint::ZERO.checked_next().unwrap();
        let microtask_checkpoint = PendingMicrotaskCheckpoint::new(1);
        let mut observer = DocumentProducerObserver::default();
        let observation = observer.observe(&fence, checkpoint).unwrap();
        PendingProducerQualificationLedger::new(TEST_SCRIPT_EVENT_LOOP_ID)
            .qualify(
                microtask_checkpoint,
                checkpoint,
                observation,
                fence.snapshot(),
            )
            .unwrap()
    }

    fn build_facts(
        ledger: &PendingStateLedger,
        producers: PendingProducerObservation,
        now: DocumentTime,
    ) -> RawPendingBuildFacts {
        let clock = DocumentClock::new(DocumentClockConfiguration::Controlled {
            initial_time_ns: now.as_nanos(),
            unix_time_origin_ns: DocumentUnixTime::default(),
        });
        let scheduler = TimerScheduler::with_clock(clock.clone());
        RawPendingBuildFacts {
            target: target(),
            owner: ledger.owner_snapshot(TEST_WEBVIEW_ID).unwrap(),
            dom_epoch: DomEpoch::ZERO,
            clock: PendingClockFacts {
                observation: PendingClockObservation {
                    clock_id: clock.id(),
                    mode: PendingClockMode::Controlled,
                    now,
                    unsupported_surface: None,
                },
                terminal: None,
            },
            scheduler: PendingSchedulerFacts {
                observation: PendingSchedulerObservation {
                    scheduler_id: scheduler.id(),
                    next_deadline: None,
                },
                terminal: None,
            },
            input: PendingInputFacts::default(),
            microtasks: PendingMicrotaskFacts {
                queued: 0,
                completed_checkpoint: producers.microtask_checkpoint,
                checkpoint_in_progress: false,
                terminal: None,
            },
            execution: Some(DocumentExecutionObservation {
                clock_id: clock.id(),
                limits: DocumentExecutionLimits::CONTROLLED_WEBAPP_V1,
                counters: DocumentExecutionCounters::default(),
                terminal: None,
            }),
            producers,
            rendering: Some(rendering()),
            supplemental_terminals: PendingRuntimeTerminals::default(),
        }
    }

    fn parser_facts(owner_id: u64, phase: PendingParserPhase) -> PendingParserFacts {
        PendingParserFacts {
            owner_id: PendingParserOwnerId::try_new(owner_id).unwrap(),
            pipeline_id: TEST_PIPELINE_ID,
            kind: PendingParserSourceKind::DocumentParser,
            phase,
            disposition: PendingSourceDisposition::Ready,
        }
    }

    fn persistent_source(
        stable_id: PendingPersistentSourceStableId,
    ) -> PendingPersistentSourceIdentity {
        PendingPersistentSourceIdentity {
            pipeline_id: TEST_PIPELINE_ID,
            stable_id,
        }
    }

    #[test]
    fn complete_source_authority_requires_every_typed_inventory_and_exact_fence() {
        let producers = empty_producers();
        let mut ledger = ledger();
        assert!(
            ledger
                .owner_snapshot(TEST_WEBVIEW_ID)
                .unwrap()
                .sources
                .is_none()
        );

        ledger
            .replace_logical_timers(TEST_WEBVIEW_ID, Vec::new())
            .unwrap();
        assert!(
            ledger
                .owner_snapshot(TEST_WEBVIEW_ID)
                .unwrap()
                .logical_timers
                .is_some()
        );
        assert!(
            ledger
                .owner_snapshot(TEST_WEBVIEW_ID)
                .unwrap()
                .sources
                .is_none()
        );

        ledger.replace_parsers(TEST_WEBVIEW_ID, Vec::new()).unwrap();
        assert!(
            ledger
                .owner_snapshot(TEST_WEBVIEW_ID)
                .unwrap()
                .parser
                .is_some()
        );
        assert!(
            ledger
                .owner_snapshot(TEST_WEBVIEW_ID)
                .unwrap()
                .sources
                .is_none()
        );

        ledger
            .replace_persistent_sources(TEST_WEBVIEW_ID, Vec::new())
            .unwrap();
        assert!(
            ledger
                .owner_snapshot(TEST_WEBVIEW_ID)
                .unwrap()
                .sources
                .is_none()
        );
        let source_epoch = ledger.webview(TEST_WEBVIEW_ID).unwrap().source_epoch;

        ledger
            .bind_resource_fence_network_authority(TEST_WEBVIEW_ID, producers.fence_id)
            .unwrap();
        let complete = ledger.owner_snapshot(TEST_WEBVIEW_ID).unwrap();
        assert!(complete.sources.is_some());
        assert!(complete.network.is_some());
        assert_eq!(
            ledger.webview(TEST_WEBVIEW_ID).unwrap().source_epoch,
            source_epoch
        );
        let generation = complete.state_generation;
        ledger
            .bind_resource_fence_network_authority(TEST_WEBVIEW_ID, producers.fence_id)
            .unwrap();
        assert_eq!(
            ledger
                .owner_snapshot(TEST_WEBVIEW_ID)
                .unwrap()
                .state_generation,
            generation
        );
    }

    #[test]
    fn parser_replacement_is_transactional_stable_and_aba_safe() {
        let mut ledger = ledger();
        let first = parser_facts(1, PendingParserPhase::Ready);
        ledger
            .replace_parsers(TEST_WEBVIEW_ID, vec![first])
            .unwrap();
        let first_key = PendingStableSourceKey::Parser(first.into());
        let first_source = ledger.webview(TEST_WEBVIEW_ID).unwrap().source_keys[&first_key];
        let first_epoch = ledger.webview(TEST_WEBVIEW_ID).unwrap().source_epoch;

        let phase_only = parser_facts(1, PendingParserPhase::AwaitingCommit);
        ledger
            .replace_parsers(TEST_WEBVIEW_ID, vec![phase_only])
            .unwrap();
        let phase_state = ledger.webview(TEST_WEBVIEW_ID).unwrap();
        assert_eq!(phase_state.source_keys[&first_key], first_source);
        assert_eq!(phase_state.source_epoch, first_epoch);

        let forged = PendingParserIdentity {
            source_id: first_source,
            owner_id: PendingParserOwnerId::try_new(2).unwrap(),
            pipeline_id: TEST_PIPELINE_ID,
            kind: PendingParserSourceKind::DocumentParser,
        };
        let before_forged_update = ledger.webview(TEST_WEBVIEW_ID).unwrap().clone();
        assert_eq!(
            ledger.update_parser(
                TEST_WEBVIEW_ID,
                forged,
                PendingParserPhase::Ready,
                PendingSourceDisposition::Ready,
            ),
            Err(PendingStateError::UnknownParser(forged))
        );
        assert_eq!(
            ledger.webview(TEST_WEBVIEW_ID).unwrap().source_keys,
            before_forged_update.source_keys
        );

        let replacement = parser_facts(2, PendingParserPhase::Ready);
        ledger
            .replace_parsers(TEST_WEBVIEW_ID, vec![replacement])
            .unwrap();
        let replacement_key = PendingStableSourceKey::Parser(replacement.into());
        let replacement_source =
            ledger.webview(TEST_WEBVIEW_ID).unwrap().source_keys[&replacement_key];
        assert_ne!(replacement_source, first_source);
        assert!(
            !ledger
                .webview(TEST_WEBVIEW_ID)
                .unwrap()
                .sources
                .contains_key(&first_source)
        );

        let before = ledger.webview(TEST_WEBVIEW_ID).unwrap().clone();
        let before_last_source = ledger.last_source_id;
        assert_eq!(
            ledger.replace_parsers(TEST_WEBVIEW_ID, vec![replacement, replacement]),
            Err(PendingStateError::DuplicateParserOwner(
                replacement.owner_id
            ))
        );
        assert_eq!(
            ledger.webview(TEST_WEBVIEW_ID).unwrap().source_keys,
            before.source_keys
        );
        assert_eq!(ledger.last_source_id, before_last_source);

        let invalid = PendingParserFacts {
            phase: PendingParserPhase::Suspended,
            ..replacement
        };
        assert!(matches!(
            ledger.replace_parsers(TEST_WEBVIEW_ID, vec![invalid]),
            Err(PendingStateError::SnapshotInvariant(
                PendingSnapshotInvariantError::ParserPhaseDispositionMismatch(_)
            ))
        ));
        assert_eq!(
            ledger.webview(TEST_WEBVIEW_ID).unwrap().source_keys,
            before.source_keys
        );
        assert_eq!(ledger.last_source_id, before_last_source);

        ledger.replace_parsers(TEST_WEBVIEW_ID, Vec::new()).unwrap();
        ledger
            .replace_parsers(TEST_WEBVIEW_ID, vec![replacement])
            .unwrap();
        assert_ne!(
            ledger.webview(TEST_WEBVIEW_ID).unwrap().source_keys[&replacement_key],
            replacement_source
        );
    }

    #[test]
    fn persistent_replacement_derives_classification_and_rejects_invalid_batches() {
        let identities = vec![
            persistent_source(PendingPersistentSourceStableId::WebSocket(1)),
            persistent_source(PendingPersistentSourceStableId::EventSource(2)),
            persistent_source(PendingPersistentSourceStableId::BroadcastChannel(3)),
            persistent_source(PendingPersistentSourceStableId::MediaSessionActionHandler),
            persistent_source(PendingPersistentSourceStableId::StorageEventListener),
            persistent_source(PendingPersistentSourceStableId::Worker),
        ];
        let mut ledger = ledger();
        ledger
            .replace_persistent_sources(TEST_WEBVIEW_ID, identities.clone())
            .unwrap();
        let state = ledger.webview(TEST_WEBVIEW_ID).unwrap();
        for identity in &identities {
            let source_id = state.source_keys[&PendingStableSourceKey::Persistent(*identity)];
            let source = state.sources[&source_id];
            assert_eq!(source.kind, identity.source_kind());
            assert_eq!(source.disposition, identity.source_disposition());
        }
        let before = state.clone();
        let before_last_source = ledger.last_source_id;

        let mut reordered = identities.clone();
        reordered.reverse();
        ledger
            .replace_persistent_sources(TEST_WEBVIEW_ID, reordered)
            .unwrap();
        assert_eq!(
            ledger.webview(TEST_WEBVIEW_ID).unwrap().state_generation,
            before.state_generation
        );
        assert_eq!(ledger.last_source_id, before_last_source);

        let invalid = persistent_source(PendingPersistentSourceStableId::WebSocket(0));
        assert_eq!(
            ledger.replace_persistent_sources(TEST_WEBVIEW_ID, vec![invalid]),
            Err(PendingStateError::InvalidPersistentSource(invalid))
        );
        assert_eq!(
            ledger.webview(TEST_WEBVIEW_ID).unwrap().source_keys,
            before.source_keys
        );
        assert_eq!(ledger.last_source_id, before_last_source);

        let duplicate = identities[0];
        assert_eq!(
            ledger.replace_persistent_sources(TEST_WEBVIEW_ID, vec![duplicate, duplicate]),
            Err(PendingStateError::DuplicatePersistentSource(duplicate))
        );
        assert_eq!(
            ledger.webview(TEST_WEBVIEW_ID).unwrap().source_keys,
            before.source_keys
        );
        assert_eq!(ledger.last_source_id, before_last_source);
    }

    #[test]
    fn fence_binding_is_immutable_and_normalization_rejects_spliced_observation() {
        let first = empty_producers();
        let second = empty_producers();
        let mut ledger = complete_ledger(first);
        let before = ledger.webview(TEST_WEBVIEW_ID).unwrap().clone();
        assert_eq!(
            ledger.bind_resource_fence_network_authority(TEST_WEBVIEW_ID, second.fence_id),
            Err(PendingStateError::ResourceFenceAlreadyBound {
                expected: first.fence_id,
                observed: second.fence_id,
            })
        );
        assert_eq!(
            ledger
                .webview(TEST_WEBVIEW_ID)
                .unwrap()
                .resource_fence_authority,
            before.resource_fence_authority
        );

        assert_eq!(
            ledger.normalize_and_build(build_facts(&ledger, second, DocumentTime::from_nanos(5),)),
            Err(PendingNormalizeError::ResourceFenceAuthorityMismatch {
                expected: first.fence_id,
                observed: second.fence_id,
            })
        );
        let after = ledger.webview(TEST_WEBVIEW_ID).unwrap();
        assert_eq!(after.source_keys, before.source_keys);
        assert_eq!(after.resource_fallback, before.resource_fallback);
        assert_eq!(
            after.last_normalized_snapshot,
            before.last_normalized_snapshot
        );

        let other_webview = WebViewId::mock_for_testing(BrowsingContextId {
            namespace_id: PipelineNamespaceId(4321),
            index: Index::<BrowsingContextIndex>::new(7654).unwrap(),
        });
        ledger.register_webview(other_webview).unwrap();
        assert_eq!(
            ledger.bind_resource_fence_network_authority(other_webview, second.fence_id),
            Err(PendingStateError::ResourceFenceAlreadyBound {
                expected: first.fence_id,
                observed: second.fence_id,
            })
        );
        assert_eq!(
            ledger
                .webview(other_webview)
                .unwrap()
                .resource_fence_authority,
            None
        );
    }

    #[test]
    fn persistent_source_outside_target_is_rejected_before_normalization_mutates_owner() {
        let producers = empty_producers();
        let mut ledger = complete_ledger(producers);
        let outside = PendingPersistentSourceIdentity {
            pipeline_id: PipelineId {
                namespace_id: PipelineNamespaceId(1234),
                index: Index::<PipelineIndex>::new(9999).unwrap(),
            },
            stable_id: PendingPersistentSourceStableId::WebSocket(1),
        };
        ledger
            .replace_persistent_sources(TEST_WEBVIEW_ID, vec![outside])
            .unwrap();
        let before = ledger.webview(TEST_WEBVIEW_ID).unwrap().clone();

        assert_eq!(
            ledger.normalize_and_build(build_facts(
                &ledger,
                producers,
                DocumentTime::from_nanos(5),
            )),
            Err(PendingNormalizeError::PersistentSourceOutsideTarget(
                outside
            ))
        );
        let after = ledger.webview(TEST_WEBVIEW_ID).unwrap();
        assert_eq!(after.source_keys, before.source_keys);
        assert_eq!(after.resource_fallback, before.resource_fallback);
        assert_eq!(
            after.last_normalized_snapshot,
            before.last_normalized_snapshot
        );
    }

    #[test]
    fn remove_and_readd_allocates_a_new_source_identity_and_epoch() {
        let mut ledger = ledger();
        let facts = timer_facts(7);
        ledger
            .replace_logical_timers(TEST_WEBVIEW_ID, vec![facts])
            .unwrap();
        let first = *ledger
            .webview(TEST_WEBVIEW_ID)
            .unwrap()
            .source_keys
            .get(&PendingStableSourceKey::LogicalTimer(facts.identity))
            .unwrap();
        let first_epoch = ledger.webview(TEST_WEBVIEW_ID).unwrap().source_epoch;

        ledger
            .replace_logical_timers(TEST_WEBVIEW_ID, Vec::new())
            .unwrap();
        ledger
            .replace_logical_timers(TEST_WEBVIEW_ID, vec![facts])
            .unwrap();
        let second = *ledger
            .webview(TEST_WEBVIEW_ID)
            .unwrap()
            .source_keys
            .get(&PendingStableSourceKey::LogicalTimer(facts.identity))
            .unwrap();
        let second_epoch = ledger.webview(TEST_WEBVIEW_ID).unwrap().source_epoch;

        assert_ne!(first, second);
        assert!(second_epoch > first_epoch);
    }

    #[test]
    fn live_timer_and_parser_keys_keep_their_source_identity() {
        let mut ledger = ledger();
        let timer = timer_facts(7);
        ledger
            .replace_logical_timers(TEST_WEBVIEW_ID, vec![timer])
            .unwrap();
        let timer_id = *ledger
            .webview(TEST_WEBVIEW_ID)
            .unwrap()
            .source_keys
            .get(&PendingStableSourceKey::LogicalTimer(timer.identity))
            .unwrap();
        ledger
            .replace_logical_timers(TEST_WEBVIEW_ID, vec![timer_facts(8)])
            .unwrap();
        assert_eq!(
            ledger
                .webview(TEST_WEBVIEW_ID)
                .unwrap()
                .source_keys
                .get(&PendingStableSourceKey::LogicalTimer(timer.identity)),
            Some(&timer_id)
        );
        assert_eq!(
            ledger
                .webview(TEST_WEBVIEW_ID)
                .unwrap()
                .logical_timers
                .get(&timer_id)
                .unwrap()
                .creation_sequence,
            8
        );

        let parser = ledger
            .start_parser(
                TEST_WEBVIEW_ID,
                TEST_PIPELINE_ID,
                PendingParserSourceKind::DocumentParser,
                PendingParserPhase::Ready,
                PendingSourceDisposition::Ready,
            )
            .unwrap();
        ledger
            .update_parser(
                TEST_WEBVIEW_ID,
                parser,
                PendingParserPhase::AwaitingCommit,
                PendingSourceDisposition::Ready,
            )
            .unwrap();
        assert_eq!(
            ledger
                .webview(TEST_WEBVIEW_ID)
                .unwrap()
                .parsers
                .get(&parser.source_id)
                .unwrap()
                .source_id,
            parser.source_id
        );
    }

    #[test]
    fn event_source_reconnect_is_open_ended_not_a_finite_deadline() {
        let mut ledger = ledger();
        let timer = active_timer_facts(
            PendingLogicalTimerStableId::EngineHandle(19),
            1,
            PendingLogicalTimerKind::EventSourceReconnect,
        );
        ledger
            .replace_logical_timers(TEST_WEBVIEW_ID, vec![timer])
            .unwrap();
        let source_id = *ledger
            .webview(TEST_WEBVIEW_ID)
            .unwrap()
            .source_keys
            .get(&PendingStableSourceKey::LogicalTimer(timer.identity))
            .unwrap();

        assert_eq!(
            ledger
                .webview(TEST_WEBVIEW_ID)
                .unwrap()
                .sources
                .get(&source_id)
                .unwrap()
                .disposition,
            PendingSourceDisposition::OpenEnded(PendingOpenEndedSourceReason::EventSource)
        );
    }

    #[test]
    fn complete_timer_replacement_preserves_ids_removes_absent_and_rolls_back_invalid_input() {
        let mut ledger = ledger();
        let first = timer_facts(1);
        ledger
            .replace_logical_timers(TEST_WEBVIEW_ID, vec![first])
            .unwrap();
        let source_id = ledger
            .webview(TEST_WEBVIEW_ID)
            .unwrap()
            .source_keys
            .get(&PendingStableSourceKey::LogicalTimer(first.identity))
            .copied()
            .unwrap();

        ledger
            .replace_logical_timers(TEST_WEBVIEW_ID, vec![timer_facts(2)])
            .unwrap();
        assert_eq!(
            ledger
                .webview(TEST_WEBVIEW_ID)
                .unwrap()
                .source_keys
                .get(&PendingStableSourceKey::LogicalTimer(first.identity)),
            Some(&source_id)
        );

        let before = ledger.owner_snapshot(TEST_WEBVIEW_ID).unwrap();
        let invalid = PendingLogicalTimerFacts {
            suspended: false,
            is_ordering_head: false,
            ..timer_facts(3)
        };
        assert!(matches!(
            ledger.replace_logical_timers(TEST_WEBVIEW_ID, vec![invalid]),
            Err(PendingStateError::SnapshotInvariant(
                PendingSnapshotInvariantError::LogicalTimerOwnerHeadCount { .. }
            ))
        ));
        assert_eq!(ledger.owner_snapshot(TEST_WEBVIEW_ID).unwrap(), before);
        let mismatched_identity_kind = PendingLogicalTimerFacts {
            kind: PendingLogicalTimerKind::XmlHttpRequestTimeout,
            ..timer_facts(4)
        };
        assert!(matches!(
            ledger.replace_logical_timers(TEST_WEBVIEW_ID, vec![mismatched_identity_kind]),
            Err(PendingStateError::SnapshotInvariant(
                PendingSnapshotInvariantError::LogicalTimerStableIdentityKindMismatch { .. }
            ))
        ));
        assert_eq!(ledger.owner_snapshot(TEST_WEBVIEW_ID).unwrap(), before);

        ledger
            .replace_logical_timers(TEST_WEBVIEW_ID, Vec::new())
            .unwrap();
        let state = ledger.webview(TEST_WEBVIEW_ID).unwrap();
        assert!(state.logical_timers.is_empty());
        assert!(!state.sources.contains_key(&source_id));
    }

    #[test]
    fn checked_owner_counters_latch_without_wrapping() {
        let mut source_ids = ledger();
        source_ids.last_source_id = PendingSourceId::new(u64::MAX);
        assert_eq!(
            source_ids.replace_logical_timers(TEST_WEBVIEW_ID, vec![timer_facts(7)]),
            Err(PendingStateError::SourceIdExhausted)
        );
        assert_eq!(
            source_ids.source_id_terminal,
            Some(PendingSourceIdTerminalObservation {
                event_loop_id: TEST_SCRIPT_EVENT_LOOP_ID,
                last_issued: PendingSourceId::new(u64::MAX),
                error: PendingGenerationTerminal::Exhausted,
            })
        );

        let mut state_generations = ledger();
        state_generations
            .webviews
            .get_mut(&TEST_WEBVIEW_ID)
            .unwrap()
            .state_generation = RuntimeStateGeneration::new(u64::MAX);
        assert_eq!(
            state_generations.replace_persistent_sources(TEST_WEBVIEW_ID, Vec::new()),
            Err(PendingStateError::StateGenerationExhausted(TEST_WEBVIEW_ID))
        );
        assert!(
            state_generations
                .webviews
                .get(&TEST_WEBVIEW_ID)
                .unwrap()
                .state_generation_terminal
                .is_some()
        );

        let mut source_epochs = ledger();
        source_epochs
            .webviews
            .get_mut(&TEST_WEBVIEW_ID)
            .unwrap()
            .source_epoch = PendingSourceEpoch::new(u64::MAX);
        assert_eq!(
            source_epochs.replace_logical_timers(TEST_WEBVIEW_ID, vec![timer_facts(7)]),
            Err(PendingStateError::SourceEpochExhausted(TEST_WEBVIEW_ID))
        );
        assert!(
            source_epochs
                .webviews
                .get(&TEST_WEBVIEW_ID)
                .unwrap()
                .source_epoch_terminal
                .is_some()
        );
    }

    #[test]
    fn timer_batch_source_capacity_is_preflighted_without_mutating_the_allocator_or_owner() {
        let mut ledger = ledger();
        ledger.last_source_id = PendingSourceId::new(u64::MAX - 1);
        let second = PendingLogicalTimerFacts {
            identity: PendingLogicalTimerIdentity {
                pipeline_id: TEST_PIPELINE_ID,
                stable_id: PendingLogicalTimerStableId::JavaScriptHandle(8),
            },
            creation_sequence: 8,
            ..timer_facts(7)
        };
        let before = ledger.webview(TEST_WEBVIEW_ID).unwrap().clone();

        assert_eq!(
            ledger.replace_logical_timers(TEST_WEBVIEW_ID, vec![timer_facts(7), second],),
            Err(PendingStateError::SourceIdExhausted)
        );
        let after = ledger.webview(TEST_WEBVIEW_ID).unwrap();
        assert_eq!(after.state_generation, before.state_generation);
        assert_eq!(after.source_epoch, before.source_epoch);
        assert_eq!(after.source_keys, before.source_keys);
        assert_eq!(after.sources, before.sources);
        assert_eq!(after.logical_timers, before.logical_timers);
        assert_eq!(ledger.last_source_id, PendingSourceId::new(u64::MAX - 1));
        assert_eq!(ledger.source_id_terminal, None);

        ledger
            .replace_logical_timers(TEST_WEBVIEW_ID, vec![timer_facts(7)])
            .unwrap();
        assert_eq!(ledger.last_source_id, PendingSourceId::new(u64::MAX));
        assert_eq!(ledger.source_id_terminal, None);
    }

    #[test]
    fn producer_begin_and_finish_after_checkpoint_is_unqualified() {
        let fence = DocumentProducerFence::default();
        let checkpoint = DocumentProducerCheckpoint::ZERO.checked_next().unwrap();
        let mut observer = DocumentProducerObserver::default();
        let observed = observer.observe(&fence, checkpoint).unwrap();
        let producer = fence.begin(DocumentProducerKind::Task).unwrap();
        drop(producer);

        let pending = PendingProducerQualificationLedger::new(TEST_SCRIPT_EVENT_LOOP_ID)
            .qualify(
                PendingMicrotaskCheckpoint::new(1),
                checkpoint,
                observed,
                fence.snapshot(),
            )
            .unwrap();

        assert_eq!(pending.stability, PendingProducerStability::Unqualified);
        assert!(pending.snapshot.is_empty());
        assert_eq!(pending.snapshot.revision(), 2);
    }

    #[test]
    fn passive_capture_preserves_only_exact_checkpoint_qualification() {
        let fence = DocumentProducerFence::default();
        let mut observer = DocumentProducerObserver::default();
        let mut qualification = PendingProducerQualificationLedger::new(TEST_SCRIPT_EVENT_LOOP_ID);
        let first_checkpoint = DocumentProducerCheckpoint::ZERO.checked_next().unwrap();
        let first = qualification
            .qualify(
                PendingMicrotaskCheckpoint::new(1),
                first_checkpoint,
                observer.observe(&fence, first_checkpoint).unwrap(),
                fence.snapshot(),
            )
            .unwrap();
        assert_eq!(first.stability, PendingProducerStability::FirstEmpty);

        let stable_checkpoint = first_checkpoint.checked_next().unwrap();
        let stable = qualification
            .qualify(
                PendingMicrotaskCheckpoint::new(2),
                stable_checkpoint,
                observer.observe(&fence, stable_checkpoint).unwrap(),
                fence.snapshot(),
            )
            .unwrap();
        assert_eq!(stable.stability, PendingProducerStability::StableEmpty);

        let repeated = qualification
            .passive(
                PendingMicrotaskCheckpoint::new(2),
                stable_checkpoint,
                fence.snapshot(),
            )
            .unwrap();
        assert_eq!(repeated, stable);

        let mut pending = complete_ledger(stable);
        let first_facts = build_facts(&pending, stable, DocumentTime::ZERO);
        let first_snapshot = pending.normalize_and_build(first_facts.clone()).unwrap();
        let mut repeated_facts = first_facts;
        repeated_facts.owner = pending.owner_snapshot(TEST_WEBVIEW_ID).unwrap();
        repeated_facts.producers = repeated;
        let repeated_snapshot = pending.normalize_and_build(repeated_facts.clone()).unwrap();
        assert_eq!(
            repeated_snapshot.state_generation,
            first_snapshot.state_generation
        );

        let unqualified = qualification
            .passive(
                PendingMicrotaskCheckpoint::new(3),
                stable_checkpoint,
                fence.snapshot(),
            )
            .unwrap();
        assert_eq!(unqualified.stability, PendingProducerStability::Unqualified);
        repeated_facts.owner = pending.owner_snapshot(TEST_WEBVIEW_ID).unwrap();
        repeated_facts.producers = unqualified;
        repeated_facts.microtasks.completed_checkpoint = PendingMicrotaskCheckpoint::new(3);
        let changed_snapshot = pending.normalize_and_build(repeated_facts).unwrap();
        assert!(changed_snapshot.state_generation > repeated_snapshot.state_generation);
    }

    #[test]
    fn passive_capture_rejects_changed_fence_revision() {
        let fence = DocumentProducerFence::default();
        let mut observer = DocumentProducerObserver::default();
        let mut qualification = PendingProducerQualificationLedger::new(TEST_SCRIPT_EVENT_LOOP_ID);
        let first_checkpoint = DocumentProducerCheckpoint::ZERO.checked_next().unwrap();
        qualification
            .qualify(
                PendingMicrotaskCheckpoint::new(1),
                first_checkpoint,
                observer.observe(&fence, first_checkpoint).unwrap(),
                fence.snapshot(),
            )
            .unwrap();
        let stable_checkpoint = first_checkpoint.checked_next().unwrap();
        qualification
            .qualify(
                PendingMicrotaskCheckpoint::new(2),
                stable_checkpoint,
                observer.observe(&fence, stable_checkpoint).unwrap(),
                fence.snapshot(),
            )
            .unwrap();

        let producer = fence.begin(DocumentProducerKind::Task).unwrap();
        drop(producer);
        let unqualified = qualification
            .passive(
                PendingMicrotaskCheckpoint::new(2),
                stable_checkpoint,
                fence.snapshot(),
            )
            .unwrap();
        assert_eq!(unqualified.stability, PendingProducerStability::Unqualified);
        assert_eq!(unqualified.snapshot.revision(), 2);
    }

    #[test]
    fn pure_builder_requires_every_authoritative_inventory() {
        let producers = empty_producers();
        let ledger = complete_ledger(producers);
        let mut facts = build_facts(&ledger, producers, DocumentTime::from_nanos(5));
        facts.owner.logical_timers = None;

        assert_eq!(
            RawPendingBuilder::build(facts.clone()),
            Err(PendingBuildError::MissingFact(
                PendingFactKind::LogicalTimers
            ))
        );
        facts.owner = ledger.owner_snapshot(TEST_WEBVIEW_ID).unwrap();
        facts.rendering = None;

        assert_eq!(
            RawPendingBuilder::build(facts),
            Err(PendingBuildError::MissingFact(PendingFactKind::Rendering))
        );
    }

    #[test]
    fn owner_normalization_advances_generation_for_nonledger_fact_changes() {
        let producers = empty_producers();
        let mut ledger = complete_ledger(producers);
        let first_facts = build_facts(&ledger, producers, DocumentTime::from_nanos(5));
        let first = ledger.normalize_and_build(first_facts.clone()).unwrap();
        let mut second_facts = first_facts;
        second_facts.owner = ledger.owner_snapshot(TEST_WEBVIEW_ID).unwrap();
        second_facts.clock.observation.now = DocumentTime::from_nanos(6);
        let second = ledger.normalize_and_build(second_facts.clone()).unwrap();
        second_facts.owner = ledger.owner_snapshot(TEST_WEBVIEW_ID).unwrap();
        let unchanged = ledger.normalize_and_build(second_facts.clone()).unwrap();

        assert!(second.state_generation > first.state_generation);
        assert_eq!(unchanged.state_generation, second.state_generation);

        let mut execution_facts = second_facts;
        execution_facts.owner = ledger.owner_snapshot(TEST_WEBVIEW_ID).unwrap();
        execution_facts.execution = Some(DocumentExecutionObservation {
            clock_id: execution_facts.clock.observation.clock_id,
            limits: DocumentExecutionLimits::CONTROLLED_WEBAPP_V1,
            counters: DocumentExecutionCounters {
                ordinary_tasks: 1,
                ..DocumentExecutionCounters::default()
            },
            terminal: None,
        });
        let execution_changed = ledger.normalize_and_build(execution_facts).unwrap();
        assert!(execution_changed.state_generation > unchanged.state_generation);
    }

    #[test]
    fn unknown_resource_producer_gets_stable_external_io_fallback() {
        let fence = DocumentProducerFence::default();
        let resource = fence.begin(DocumentProducerKind::Resource).unwrap();
        let checkpoint = DocumentProducerCheckpoint::ZERO.checked_next().unwrap();
        let mut observer = DocumentProducerObserver::default();
        let observed = observer.observe(&fence, checkpoint).unwrap();
        let mut qualification = PendingProducerQualificationLedger::new(TEST_SCRIPT_EVENT_LOOP_ID);
        let busy = qualification
            .qualify(
                PendingMicrotaskCheckpoint::new(1),
                checkpoint,
                observed,
                fence.snapshot(),
            )
            .unwrap();
        let mut ledger = complete_ledger(busy);
        let websocket = persistent_source(PendingPersistentSourceStableId::WebSocket(91));
        ledger
            .replace_persistent_sources(TEST_WEBVIEW_ID, vec![websocket])
            .unwrap();

        let first = ledger
            .normalize_and_build(build_facts(&ledger, busy, DocumentTime::from_nanos(5)))
            .unwrap();
        let fallback_source = first.network.active()[0].source_id;
        assert_eq!(first.network.active().len(), 1);
        assert_eq!(
            first.network.active()[0].kind,
            PendingNetworkKind::ProducerFallback
        );
        assert_eq!(
            first.network.active()[0].evidence,
            embedder_traits::document_pending::PendingExternalIoEvidence {
                owner: PendingExternalIoOwner::Other,
                load_blocking: PendingExternalIoLoadBlocking::Unknown,
            }
        );
        assert!(first.sources.sources().iter().any(|source| {
            source.kind == PendingSourceKind::Network
                && source.disposition
                    == PendingSourceDisposition::OpenEnded(PendingOpenEndedSourceReason::WebSocket)
        }));

        let repeated = ledger
            .normalize_and_build(build_facts(&ledger, busy, DocumentTime::from_nanos(5)))
            .unwrap();
        assert_eq!(repeated.network.active()[0].source_id, fallback_source);

        drop(resource);
        let checkpoint = checkpoint.checked_next().unwrap();
        let observed = observer.observe(&fence, checkpoint).unwrap();
        let empty = qualification
            .qualify(
                PendingMicrotaskCheckpoint::new(2),
                checkpoint,
                observed,
                fence.snapshot(),
            )
            .unwrap();
        let settled = ledger
            .normalize_and_build(build_facts(&ledger, empty, DocumentTime::from_nanos(5)))
            .unwrap();
        assert!(settled.network.active().is_empty());
        assert!(
            settled
                .sources
                .sources()
                .iter()
                .all(|source| source.id != fallback_source)
        );
    }

    #[test]
    fn invalid_capture_does_not_mutate_resource_fallback_or_owner_counters() {
        let fence = DocumentProducerFence::default();
        let _resource = fence.begin(DocumentProducerKind::Resource).unwrap();
        let checkpoint = DocumentProducerCheckpoint::ZERO.checked_next().unwrap();
        let mut observer = DocumentProducerObserver::default();
        let observed = observer.observe(&fence, checkpoint).unwrap();
        let busy = PendingProducerQualificationLedger::new(TEST_SCRIPT_EVENT_LOOP_ID)
            .qualify(
                PendingMicrotaskCheckpoint::new(1),
                checkpoint,
                observed,
                fence.snapshot(),
            )
            .unwrap();
        let mut ledger = complete_ledger(busy);
        let before_owner = ledger.owner_snapshot(TEST_WEBVIEW_ID).unwrap();
        let before_source_id = ledger.last_source_id;
        let mut facts = build_facts(&ledger, busy, DocumentTime::from_nanos(5));
        facts.rendering = None;

        assert_eq!(
            ledger.normalize_and_build(facts),
            Err(PendingNormalizeError::Build(
                PendingBuildError::MissingFact(PendingFactKind::Rendering)
            ))
        );
        assert_eq!(
            ledger.owner_snapshot(TEST_WEBVIEW_ID).unwrap(),
            before_owner
        );
        assert_eq!(ledger.last_source_id, before_source_id);
        assert!(
            ledger
                .webview(TEST_WEBVIEW_ID)
                .unwrap()
                .resource_fallback
                .is_none()
        );
    }

    #[test]
    fn resource_fallback_rehomes_across_target_pipeline_replacement() {
        let fence = DocumentProducerFence::default();
        let resource = fence.begin(DocumentProducerKind::Resource).unwrap();
        let checkpoint = DocumentProducerCheckpoint::ZERO.checked_next().unwrap();
        let mut observer = DocumentProducerObserver::default();
        let observed = observer.observe(&fence, checkpoint).unwrap();
        let mut qualification = PendingProducerQualificationLedger::new(TEST_SCRIPT_EVENT_LOOP_ID);
        let busy = qualification
            .qualify(
                PendingMicrotaskCheckpoint::new(1),
                checkpoint,
                observed,
                fence.snapshot(),
            )
            .unwrap();
        let mut ledger = complete_ledger(busy);
        let first = ledger
            .normalize_and_build(build_facts(&ledger, busy, DocumentTime::ZERO))
            .unwrap();
        let source_id = first.network.active()[0].source_id;
        let replacement_pipeline = PipelineId {
            namespace_id: PipelineNamespaceId(1234),
            index: Index::<PipelineIndex>::new(5679).unwrap(),
        };
        let mut replacement = build_facts(&ledger, busy, DocumentTime::ZERO);
        replacement.target = target_for(replacement_pipeline);
        replacement.rendering = Some(rendering_for(replacement_pipeline));

        let rehomed = ledger.normalize_and_build(replacement).unwrap();
        assert_eq!(rehomed.network.active()[0].source_id, source_id);
        assert_eq!(
            rehomed.network.active()[0].pipeline_id,
            replacement_pipeline
        );

        drop(resource);
        let checkpoint = checkpoint.checked_next().unwrap();
        let observed = observer.observe(&fence, checkpoint).unwrap();
        let empty = qualification
            .qualify(
                PendingMicrotaskCheckpoint::new(2),
                checkpoint,
                observed,
                fence.snapshot(),
            )
            .unwrap();
        let mut drained = build_facts(&ledger, empty, DocumentTime::ZERO);
        drained.target = target_for(replacement_pipeline);
        drained.rendering = Some(rendering_for(replacement_pipeline));
        let drained = ledger.normalize_and_build(drained).unwrap();
        assert!(drained.network.active().is_empty());
        assert!(
            drained
                .sources
                .sources()
                .iter()
                .all(|source| source.id != source_id)
        );
    }

    #[test]
    fn unrelated_physical_network_record_cannot_mask_resource_fallback() {
        let fence = DocumentProducerFence::default();
        let _resource = fence.begin(DocumentProducerKind::Resource).unwrap();
        let checkpoint = DocumentProducerCheckpoint::ZERO.checked_next().unwrap();
        let mut observer = DocumentProducerObserver::default();
        let observed = observer.observe(&fence, checkpoint).unwrap();
        let busy = PendingProducerQualificationLedger::new(TEST_SCRIPT_EVENT_LOOP_ID)
            .qualify(
                PendingMicrotaskCheckpoint::new(1),
                checkpoint,
                observed,
                fence.snapshot(),
            )
            .unwrap();
        let mut ledger = complete_ledger(busy);
        ledger
            .start_network(PendingNetworkStartFacts {
                webview_id: TEST_WEBVIEW_ID,
                pipeline_id: TEST_PIPELINE_ID,
                kind: PendingNetworkKind::Fetch,
                evidence: embedder_traits::document_pending::PendingExternalIoEvidence {
                    owner: PendingExternalIoOwner::Script,
                    load_blocking: PendingExternalIoLoadBlocking::NonBlocking,
                },
                started_at: DocumentTime::ZERO,
                parent: None,
            })
            .unwrap();

        let snapshot = ledger
            .normalize_and_build(build_facts(&ledger, busy, DocumentTime::ZERO))
            .unwrap();
        assert_eq!(snapshot.network.active().len(), 2);
        assert!(
            snapshot
                .network
                .active()
                .iter()
                .any(|operation| operation.kind == PendingNetworkKind::ProducerFallback)
        );
    }
}
