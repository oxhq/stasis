/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Checked same-build authority for one controlled top-level web session.
//!
//! These types are a v2-only automation seam. They deliberately do not add fields to the frozen
//! v1 pending observation, so existing controlled-webapp-v1 serialized bytes remain unchanged.

use serde::{Deserialize, Serialize};
use servo_url::ServoUrl;

use crate::document_control::DocumentControlError;
use crate::document_pending::PendingTargetObservation;

/// Maximum successful replacement documents after the initial activation.
pub const CONTROLLED_SESSION_MAX_DOCUMENT_REPLACEMENTS: u64 = 1_000;
/// Maximum admitted same-document history authority changes.
pub const CONTROLLED_SESSION_MAX_HISTORY_REVISIONS: u64 = 10_000;
/// Maximum redirects followed by one navigation. Enforcement is owned by the navigation loader.
pub const CONTROLLED_SESSION_MAX_REDIRECTS: u64 = 20;

#[cfg(test)]
mod controlled_web_session_profile_tests {
    use serde_json::Value;

    use super::{
        CONTROLLED_SESSION_MAX_DOCUMENT_REPLACEMENTS, CONTROLLED_SESSION_MAX_HISTORY_REVISIONS,
        CONTROLLED_SESSION_MAX_REDIRECTS,
    };

    #[test]
    fn frozen_profile_limits_match_the_engine_constants() {
        let profile: Value = serde_json::from_str(include_str!(
            "../../../profiles/controlled-web-session-v1.json"
        ))
        .expect("the frozen controlled-web-session-v1 profile must be valid JSON");

        assert_eq!(profile["id"], "controlled-web-session-v1");
        assert_eq!(profile["releaseStatus"], "stable_contract");
        assert_eq!(profile["targetRelease"], "0.2.0");
        assert_eq!(
            profile["navigation"]["replacementDocuments"]["maximumSuccessfulReplacements"],
            CONTROLLED_SESSION_MAX_DOCUMENT_REPLACEMENTS,
        );
        assert_eq!(
            profile["navigation"]["sameDocument"]["maximumRevisions"],
            CONTROLLED_SESSION_MAX_HISTORY_REVISIONS,
        );
        assert_eq!(
            profile["navigation"]["redirects"]["maximumFollowedHops"],
            CONTROLLED_SESSION_MAX_REDIRECTS,
        );
    }
}

macro_rules! checked_session_identity {
    ($name:ident, $docs:literal) => {
        #[doc = $docs]
        #[derive(
            Clone,
            Copy,
            Debug,
            Default,
            Deserialize,
            Eq,
            Hash,
            Ord,
            PartialEq,
            PartialOrd,
            Serialize,
        )]
        pub struct $name(u64);

        impl $name {
            /// Construct from an owner-checked sequence.
            #[doc(hidden)]
            pub const fn new(value: u64) -> Self {
                Self(value)
            }

            /// Return the underlying sequence.
            pub const fn get(self) -> u64 {
                self.0
            }
        }
    };
}

checked_session_identity!(
    DocumentEpoch,
    "Checked identity of the active top-level document in one session."
);
checked_session_identity!(
    SessionNavigationId,
    "Checked identity reserved for a top-level replacement attempt."
);
checked_session_identity!(
    HistoryRevision,
    "Checked session-monotonic same-document history authority revision."
);

/// Counter whose checked arithmetic exhausted independently from its configured product limit.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum SessionNavigationCounter {
    DocumentEpoch,
    NavigationId,
    HistoryRevision,
    SuccessfulDocumentReplacements,
}

/// Typed configured-limit failure or sticky fail-stop terminal for controlled navigation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum SessionNavigationTerminal {
    /// The next document replacement was rejected before its network request started.
    DocumentTransitionLimitExceeded {
        limit: u64,
        observed: u64,
        next_navigation_id: SessionNavigationId,
    },
    /// The next same-document history change was rejected before mutation.
    HistoryLimitExceeded {
        limit: u64,
        observed: u64,
        navigation_id: SessionNavigationId,
        history_revision: HistoryRevision,
    },
    /// A checked owner counter exhausted. The process must fail-stop.
    CounterOverflow { counter: SessionNavigationCounter },
    /// Redirect enforcement rejected a hop after network work had already occurred.
    RedirectLimitExceeded {
        limit: u64,
        observed: u64,
        navigation_id: SessionNavigationId,
    },
}

/// Engine-attested v2 session authority captured atomically by Constellation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionNavigationAuthority {
    target: Box<PendingTargetObservation>,
    document_epoch: DocumentEpoch,
    navigation_id: SessionNavigationId,
    history_revision: HistoryRevision,
    successful_document_replacements: u64,
    url: ServoUrl,
    terminal: Option<SessionNavigationTerminal>,
}

impl SessionNavigationAuthority {
    /// Construct one owner-captured session authority.
    #[doc(hidden)]
    pub fn new_internal(
        target: Box<PendingTargetObservation>,
        document_epoch: DocumentEpoch,
        navigation_id: SessionNavigationId,
        history_revision: HistoryRevision,
        successful_document_replacements: u64,
        url: ServoUrl,
        terminal: Option<SessionNavigationTerminal>,
    ) -> Self {
        Self {
            target,
            document_epoch,
            navigation_id,
            history_revision,
            successful_document_replacements,
            url,
            terminal,
        }
    }

    pub fn target(&self) -> &PendingTargetObservation {
        &self.target
    }

    pub const fn document_epoch(&self) -> DocumentEpoch {
        self.document_epoch
    }

    pub const fn navigation_id(&self) -> SessionNavigationId {
        self.navigation_id
    }

    pub const fn history_revision(&self) -> HistoryRevision {
        self.history_revision
    }

    pub const fn successful_document_replacements(&self) -> u64 {
        self.successful_document_replacements
    }

    pub fn url(&self) -> &ServoUrl {
        &self.url
    }

    pub const fn terminal(&self) -> Option<SessionNavigationTerminal> {
        self.terminal
    }
}

/// Exact classification of one response-bounded same-document session transition.
///
/// One ordinary event-loop turn can synchronously perform more than one History API mutation,
/// so a changed revision is deliberately not restricted to one successor. The checked,
/// session-monotonic revision still prevents an older or wrapped authority from being accepted.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum SameDocumentSessionTransition {
    /// Neither the session history authority nor its URL changed.
    Unchanged,
    /// One or more admitted same-document history changes completed in the bounded turn.
    HistoryChanged {
        before: HistoryRevision,
        after: HistoryRevision,
    },
}

/// Classify an owner-captured session transition without admitting document replacement drift.
///
/// `before` and `after` must name the exact same stable document target, including its checked
/// navigation and pipeline-membership revisions. Document, navigation, and replacement identity
/// cannot rotate. The history revision may remain equal or advance monotonically because one
/// bounded turn may coalesce multiple `pushState`, `replaceState`, or fragment changes. An URL
/// change without a corresponding history revision is rejected.
#[doc(hidden)]
pub fn classify_same_document_session_transition(
    before: &SessionNavigationAuthority,
    after: &SessionNavigationAuthority,
) -> Option<SameDocumentSessionTransition> {
    let target = before.target();
    let active = target.active_top_level?;
    if before.terminal().is_some()
        || after.terminal().is_some()
        || !matches!(before.url().scheme(), "http" | "https")
        || !matches!(after.url().scheme(), "http" | "https")
        || target.pipelines() != [active.pipeline_id]
        || target.fully_active_pipelines() != [active.pipeline_id]
        || !target.pending_top_level_pipelines().is_empty()
        || after.target() != target
        || after.document_epoch() != before.document_epoch()
        || after.navigation_id() != before.navigation_id()
        || after.successful_document_replacements() != before.successful_document_replacements()
        || after.history_revision() < before.history_revision()
    {
        return None;
    }

    if after.history_revision() == before.history_revision() {
        return (after.url() == before.url()).then_some(SameDocumentSessionTransition::Unchanged);
    }

    Some(SameDocumentSessionTransition::HistoryChanged {
        before: before.history_revision(),
        after: after.history_revision(),
    })
}

/// Checked failure of a v2 session observation or explicit navigation admission.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum SessionNavigationError {
    NotTopLevelSession,
    UnsupportedScheme {
        scheme: String,
    },
    NavigationInProgress,
    SourceInactive,
    NavigationStartFailed {
        observed: Box<SessionNavigationAuthority>,
    },
    ChannelClosed,
    TargetUnavailable(DocumentControlError),
    TargetChanged {
        expected: Box<SessionNavigationAuthority>,
        observed: Box<SessionNavigationAuthority>,
    },
    Terminal(SessionNavigationTerminal),
}

#[cfg(test)]
mod tests {
    use servo_base::Epoch;
    use servo_base::id::{ScriptEventLoopId, TEST_PIPELINE_ID, TEST_WEBVIEW_ID};

    use super::*;
    use crate::document_pending::{
        PendingActiveTopLevelPipeline, PendingNavigationRevision, PendingPipelineMembershipRevision,
    };

    fn stable_target(
        event_loop_id: ScriptEventLoopId,
        navigation_revision: u64,
        pipeline_membership_revision: u64,
    ) -> PendingTargetObservation {
        PendingTargetObservation::new_with_authority(
            TEST_WEBVIEW_ID,
            event_loop_id,
            Some(PendingActiveTopLevelPipeline {
                pipeline_id: TEST_PIPELINE_ID,
                epoch: Epoch(4),
            }),
            PendingNavigationRevision::new(navigation_revision),
            PendingPipelineMembershipRevision::new(pipeline_membership_revision),
            None,
            vec![TEST_PIPELINE_ID],
            vec![TEST_PIPELINE_ID],
            Vec::new(),
        )
        .unwrap()
    }

    fn authority(
        target: PendingTargetObservation,
        document_epoch: u64,
        navigation_id: u64,
        history_revision: u64,
        replacements: u64,
        url: &str,
        terminal: Option<SessionNavigationTerminal>,
    ) -> SessionNavigationAuthority {
        SessionNavigationAuthority::new_internal(
            Box::new(target),
            DocumentEpoch::new(document_epoch),
            SessionNavigationId::new(navigation_id),
            HistoryRevision::new(history_revision),
            replacements,
            ServoUrl::parse(url).unwrap(),
            terminal,
        )
    }

    fn source_authority() -> SessionNavigationAuthority {
        authority(
            stable_target(ScriptEventLoopId::new(), 3, 5),
            7,
            11,
            13,
            6,
            "https://example.test/start",
            None,
        )
    }

    #[test]
    fn v2_authority_round_trip_preserves_engine_attested_identity() {
        let target = PendingTargetObservation::new_with_authority(
            TEST_WEBVIEW_ID,
            ScriptEventLoopId::new(),
            None,
            PendingNavigationRevision::new(3),
            PendingPipelineMembershipRevision::new(5),
            None,
            vec![TEST_PIPELINE_ID],
            Vec::new(),
            vec![TEST_PIPELINE_ID],
        )
        .unwrap();
        let authority = SessionNavigationAuthority::new_internal(
            Box::new(target.clone()),
            DocumentEpoch::new(7),
            SessionNavigationId::new(11),
            HistoryRevision::new(13),
            6,
            ServoUrl::parse("https://example.test/pending").unwrap(),
            None,
        );

        let encoded = postcard::to_stdvec(&authority).unwrap();
        let decoded: SessionNavigationAuthority = postcard::from_bytes(&encoded).unwrap();
        assert_eq!(decoded, authority);
        assert_eq!(decoded.target(), &target);
        assert_eq!(decoded.document_epoch().get(), 7);
        assert_eq!(decoded.navigation_id().get(), 11);
        assert_eq!(decoded.history_revision().get(), 13);
        assert_eq!(decoded.successful_document_replacements(), 6);
        assert_eq!(decoded.url().as_str(), "https://example.test/pending");
        assert_eq!(decoded.terminal(), None);
    }

    #[test]
    fn push_replace_and_hash_drive_transitions_preserve_exact_document_authority() {
        let source = source_authority();
        for final_url in [
            "https://example.test/pushed",
            "https://example.test/replaced",
            "https://example.test/start#fragment",
        ] {
            let observed = authority(
                source.target().clone(),
                source.document_epoch().get(),
                source.navigation_id().get(),
                source.history_revision().get() + 1,
                source.successful_document_replacements(),
                final_url,
                None,
            );
            assert_eq!(
                classify_same_document_session_transition(&source, &observed),
                Some(SameDocumentSessionTransition::HistoryChanged {
                    before: source.history_revision(),
                    after: observed.history_revision(),
                }),
                "one bounded Drive must accept the supported same-document path to {final_url}",
            );
        }
    }

    #[test]
    fn one_drive_accepts_multiple_coalesced_same_document_changes() {
        let source = source_authority();
        let observed = authority(
            source.target().clone(),
            source.document_epoch().get(),
            source.navigation_id().get(),
            source.history_revision().get() + 3,
            source.successful_document_replacements(),
            "https://example.test/final#fragment",
            None,
        );

        assert_eq!(
            classify_same_document_session_transition(&source, &observed),
            Some(SameDocumentSessionTransition::HistoryChanged {
                before: source.history_revision(),
                after: observed.history_revision(),
            })
        );
    }

    #[test]
    fn unchanged_drive_requires_the_same_owner_url() {
        let source = source_authority();
        assert_eq!(
            classify_same_document_session_transition(&source, &source),
            Some(SameDocumentSessionTransition::Unchanged)
        );

        let changed_url_without_revision = authority(
            source.target().clone(),
            source.document_epoch().get(),
            source.navigation_id().get(),
            source.history_revision().get(),
            source.successful_document_replacements(),
            "https://example.test/unaccounted",
            None,
        );
        assert_eq!(
            classify_same_document_session_transition(&source, &changed_url_without_revision),
            None
        );
    }

    #[test]
    fn drive_transition_rejects_history_regression_and_target_revision_skip_or_aba() {
        let source = source_authority();
        let regressed = authority(
            source.target().clone(),
            source.document_epoch().get(),
            source.navigation_id().get(),
            source.history_revision().get() - 1,
            source.successful_document_replacements(),
            source.url().as_str(),
            None,
        );
        assert_eq!(
            classify_same_document_session_transition(&source, &regressed),
            None
        );

        let navigation_skip = authority(
            stable_target(
                source.target().event_loop_id,
                source.target().navigation_revision.get() + 2,
                source.target().pipeline_membership_revision.get(),
            ),
            source.document_epoch().get(),
            source.navigation_id().get(),
            source.history_revision().get() + 1,
            source.successful_document_replacements(),
            "https://example.test/pushed",
            None,
        );
        assert_eq!(
            classify_same_document_session_transition(&source, &navigation_skip),
            None
        );

        // The same visible pipeline set after remove/reinsert is not the same target authority.
        let membership_aba = authority(
            stable_target(
                source.target().event_loop_id,
                source.target().navigation_revision.get(),
                source.target().pipeline_membership_revision.get() + 2,
            ),
            source.document_epoch().get(),
            source.navigation_id().get(),
            source.history_revision().get() + 1,
            source.successful_document_replacements(),
            "https://example.test/pushed",
            None,
        );
        assert_eq!(
            classify_same_document_session_transition(&source, &membership_aba),
            None
        );
    }

    #[test]
    fn drive_transition_rejects_cross_target_replacement_and_terminal_drift() {
        let source = source_authority();
        let cases = [
            authority(
                stable_target(ScriptEventLoopId::new(), 3, 5),
                source.document_epoch().get(),
                source.navigation_id().get(),
                source.history_revision().get() + 1,
                source.successful_document_replacements(),
                "https://example.test/pushed",
                None,
            ),
            authority(
                source.target().clone(),
                source.document_epoch().get() + 1,
                source.navigation_id().get(),
                source.history_revision().get() + 1,
                source.successful_document_replacements() + 1,
                "https://example.test/replaced-document",
                None,
            ),
            authority(
                source.target().clone(),
                source.document_epoch().get(),
                source.navigation_id().get() + 1,
                source.history_revision().get() + 1,
                source.successful_document_replacements(),
                "https://example.test/pending-replacement",
                None,
            ),
            authority(
                source.target().clone(),
                source.document_epoch().get(),
                source.navigation_id().get(),
                source.history_revision().get(),
                source.successful_document_replacements(),
                source.url().as_str(),
                Some(SessionNavigationTerminal::CounterOverflow {
                    counter: SessionNavigationCounter::HistoryRevision,
                }),
            ),
        ];
        for observed in cases {
            assert_eq!(
                classify_same_document_session_transition(&source, &observed),
                None
            );
        }
    }
}
