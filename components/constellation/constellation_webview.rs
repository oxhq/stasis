/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::collections::VecDeque;

use embedder_traits::document_session::{
    CONTROLLED_SESSION_MAX_DOCUMENT_REPLACEMENTS, CONTROLLED_SESSION_MAX_HISTORY_REVISIONS,
    DocumentEpoch, HistoryRevision, SessionNavigationCounter, SessionNavigationId,
    SessionNavigationTerminal,
};
use embedder_traits::user_contents::UserContentManagerId;
use embedder_traits::{
    DocumentClockConfiguration, DocumentControlProfile, InputEvent, MouseLeftViewportEvent, Theme,
};
use euclid::Point2D;
use log::{debug, warn};
use rustc_hash::{FxHashMap, FxHashSet};
use script_traits::{ConstellationInputEvent, ScriptThreadMessage};
use servo_base::Epoch;
use servo_base::id::{BrowsingContextId, PipelineId, ScriptEventLoopId, WebViewId};
use servo_constellation_traits::SessionHistoryTraversalRequest;
use style_traits::CSSPixel;
use timers::DocumentTimeSurface;

use crate::browsingcontext::BrowsingContext;
use crate::pipeline::Pipeline;
use crate::session_history::{JointSessionHistory, SessionHistoryChange};

/// A terminal failure of the monotonically increasing top-level navigation revision.
///
/// Once this failure is reached, the revision stays at its last valid value and navigation
/// authority snapshots fail closed rather than observing a wrapped revision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NavigationRevisionError {
    Overflow,
}

/// A terminal failure of the complete target-pipeline membership revision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PipelineMembershipRevisionError {
    Overflow,
}

/// A canonical, side-effect-free view of the top-level navigation state owned by a WebView.
///
/// The pending pipeline ids are sorted and deduplicated so callers can compare snapshots without
/// depending on `pending_changes` insertion or `swap_remove` order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TopLevelNavigationSnapshot {
    pub active_pipeline_id: Option<PipelineId>,
    pub active_pipeline_epoch: Epoch,
    pub navigation_revision: u64,
    pub pending_pipeline_ids: Vec<PipelineId>,
}

/// The previous and next active top-level pipeline epochs produced by an activation.
pub(crate) struct TopLevelPipelineActivation {
    pub old_pipeline_id: Option<PipelineId>,
    pub old_epoch: Epoch,
    pub new_epoch: Epoch,
}

/// Recoverable failure from an application-initiated controlled-session navigation.
///
/// This is deliberately separate from sticky session authority and is reported exactly once to
/// the embedder. Configured-limit and scheme variants have no authority effect; a start failure
/// follows a successfully reserved navigation identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ApplicationSessionNavigationFailure {
    Terminal(SessionNavigationTerminal),
    UnsupportedScheme { scheme: String },
    NavigationStartFailed,
}

/// The `Constellation`'s view of a `WebView` in the embedding layer. This tracks all of the
/// `Constellation` state for this `WebView`.
pub(crate) struct ConstellationWebView {
    /// The [`WebViewId`] of this [`ConstellationWebView`].
    webview_id: WebViewId,

    /// The [`PipelineId`] of the currently active pipeline at the top level of this WebView.
    active_top_level_pipeline_id: Option<PipelineId>,

    /// A counter for changes to [`Self::active_top_level_pipeline_id`].
    active_top_level_pipeline_epoch: Epoch,

    /// A revision for changes to top-level pending membership or active pipeline identity.
    navigation_revision: u64,

    /// A sticky terminal failure that prevents revision wraparound and snapshot reuse.
    navigation_revision_failure: Option<NavigationRevisionError>,

    /// A revision for every successful insertion or removal in Constellation's pipeline map.
    pipeline_membership_revision: u64,

    /// Sticky exhaustion of [`Self::pipeline_membership_revision`].
    pipeline_membership_revision_failure: Option<PipelineMembershipRevisionError>,

    /// When a navigation is performed, we do not immediately update
    /// the session history, instead we ask the event loop to begin loading
    /// the new document, and do not update the browsing context until the
    /// document is active. Between starting the load and it activating,
    /// we store a `SessionHistoryChange` object for the navigation in progress.
    pending_changes: Vec<SessionHistoryChange>,

    /// The currently focused browsing context in this webview for key events.
    /// The focused pipeline is the current entry of the focused browsing
    /// context.
    pub focused_browsing_context_id: BrowsingContextId,

    /// The [`BrowsingContextId`] of the currently hovered browsing context, to use for
    /// knowing which frame is currently receiving cursor events.
    pub hovered_browsing_context_id: Option<BrowsingContextId>,

    /// The last mouse move point in the coordinate space of the Pipeline that it
    /// happened int.
    pub last_mouse_move_point: Point2D<f32, CSSPixel>,

    /// The joint session history for this webview.
    pub session_history: JointSessionHistory,

    /// <https://html.spec.whatwg.org/multipage/#tn-session-history-traversal-queue>
    ///
    /// A queue of traversals that should be applied sequentially. The next item from
    /// the queue is applied once [`Self::ongoing_history_traversal_request`] has finished.
    pub session_history_traversal_request_queue: VecDeque<SessionHistoryTraversalRequest>,

    /// The currently running session history traversal. This will be completed once all
    /// `Pipeline`s in a traversal become active or their load fails for some other reason.
    pub ongoing_history_traversal_request: Option<OngoingHistoryTraversalRequest>,

    /// The [`UserContentManagerId`] for all pipelines in this `WebView`. This is `Some`
    /// if the embedder has set a `UserContentManager` using the WebViewBuilder API and
    /// it is `None` otherwise.
    pub user_content_manager_id: Option<UserContentManagerId>,

    /// The immutable clock mode selected before this WebView's initial navigation.
    document_clock: DocumentClockConfiguration,

    /// The immutable top-level document authority selected independently from the clock.
    document_control_profile: DocumentControlProfile,

    /// Checked active-document identity for controlled-web-session-v1 only.
    document_epoch: u64,

    /// Checked identity reserved at each replacement navigation admission.
    session_navigation_id: u64,

    /// Checked session-monotonic same-document history authority.
    history_revision: u64,

    /// Successful replacement activations after the initial document.
    successful_document_replacements: u64,

    /// Sticky arithmetic or post-network redirect terminal.
    session_navigation_terminal: Option<SessionNavigationTerminal>,

    /// Pre-mutation failure from an application-initiated document or history change.
    ///
    /// Unlike [`Self::session_navigation_terminal`], this is not sticky authority. The rejected
    /// operation has no state effect, and passive session observation consumes the failure once.
    pending_application_navigation_failure: Option<ApplicationSessionNavigationFailure>,

    /// The single script event loop allowed to own a controlled WebView.
    controlled_event_loop_id: Option<ScriptEventLoopId>,

    /// The first unsupported surface reached by a controlled WebView.
    document_time_failure: Option<DocumentTimeSurface>,

    /// The [`Theme`] that this [`ConstellationWebView`] uses. This is communicated to all
    /// `ScriptThread`s so that they know how to render the contents of a particular `WebView.
    theme: Theme,

    /// Whether accessibility is active for this webview.
    ///
    /// Set by [`crate::Constellation::set_accessibility_active()`], and forwarded to the
    /// webview’s *active* pipelines (of those that represent documents) at any given moment
    /// via [`ScriptThreadMessage::SetAccessibilityActive`] in `set_accessibility_active()`
    /// and [`crate::Constellation::set_frame_tree_for_webview()`].
    pub accessibility_active: bool,
}

impl ConstellationWebView {
    pub(crate) fn new(
        webview_id: WebViewId,
        focused_browsing_context_id: BrowsingContextId,
        user_content_manager_id: Option<UserContentManagerId>,
        document_clock: DocumentClockConfiguration,
        document_control_profile: DocumentControlProfile,
    ) -> Self {
        Self {
            webview_id,
            user_content_manager_id,
            document_clock,
            document_control_profile,
            document_epoch: 0,
            session_navigation_id: 0,
            history_revision: 0,
            successful_document_replacements: 0,
            session_navigation_terminal: None,
            pending_application_navigation_failure: None,
            controlled_event_loop_id: None,
            document_time_failure: None,
            active_top_level_pipeline_id: None,
            active_top_level_pipeline_epoch: Epoch::default(),
            navigation_revision: 0,
            navigation_revision_failure: None,
            pipeline_membership_revision: 0,
            pipeline_membership_revision_failure: None,
            pending_changes: Default::default(),
            focused_browsing_context_id,
            hovered_browsing_context_id: None,
            last_mouse_move_point: Default::default(),
            session_history: JointSessionHistory::new(),
            session_history_traversal_request_queue: Default::default(),
            ongoing_history_traversal_request: None,
            theme: Theme::Light,
            accessibility_active: false,
        }
    }

    pub(crate) const fn document_clock(&self) -> DocumentClockConfiguration {
        self.document_clock
    }

    pub(crate) const fn document_control_profile(&self) -> DocumentControlProfile {
        self.document_control_profile
    }

    pub(crate) fn permits_session_history_traversal(&self) -> bool {
        self.document_control_profile != DocumentControlProfile::TopLevelSession
    }

    /// Reject every history-traversal ingress for the session profile and retain a target-owned
    /// unsupported terminal. Script normally rejects before sending, but embedder/devtools paths
    /// must not be able to bypass that DOM boundary and later appear quiescent.
    pub(crate) fn reject_session_history_traversal(&mut self) -> bool {
        if self.permits_session_history_traversal() {
            return false;
        }
        self.fail_document_time(DocumentTimeSurface::HistoryTraversal);
        true
    }

    pub(crate) const fn session_navigation_authority(
        &self,
    ) -> (
        DocumentEpoch,
        SessionNavigationId,
        HistoryRevision,
        u64,
        Option<SessionNavigationTerminal>,
    ) {
        (
            DocumentEpoch::new(self.document_epoch),
            SessionNavigationId::new(self.session_navigation_id),
            HistoryRevision::new(self.history_revision),
            self.successful_document_replacements,
            self.session_navigation_terminal,
        )
    }

    /// Reserve a never-reused replacement identity before network start.
    pub(crate) fn admit_session_document_replacement(
        &mut self,
    ) -> Result<SessionNavigationId, SessionNavigationTerminal> {
        if let Some(terminal) = self.session_navigation_terminal {
            return Err(terminal);
        }
        let Some(next_navigation_id) = self.session_navigation_id.checked_add(1) else {
            let terminal = SessionNavigationTerminal::CounterOverflow {
                counter: SessionNavigationCounter::NavigationId,
            };
            self.session_navigation_terminal = Some(terminal);
            return Err(terminal);
        };
        if self.successful_document_replacements >= CONTROLLED_SESSION_MAX_DOCUMENT_REPLACEMENTS {
            let terminal = SessionNavigationTerminal::DocumentTransitionLimitExceeded {
                limit: CONTROLLED_SESSION_MAX_DOCUMENT_REPLACEMENTS,
                observed: self.successful_document_replacements.saturating_add(1),
                next_navigation_id: SessionNavigationId::new(next_navigation_id),
            };
            return Err(terminal);
        }
        // Reserve only after admission. A failed configured-limit check has no state effect and
        // leaves the previous authority reusable; a later fetch failure never reuses this ID.
        self.session_navigation_id = next_navigation_id;
        Ok(SessionNavigationId::new(next_navigation_id))
    }

    /// Admit an application-initiated replacement and retain a pre-admission rejection for the
    /// next passive session observation.
    pub(crate) fn admit_application_session_document_replacement(&mut self, scheme: &str) -> bool {
        if self.document_control_profile != DocumentControlProfile::TopLevelSession {
            return false;
        }
        if self.session_navigation_terminal.is_some() {
            return false;
        }
        if self.reject_application_session_navigation_scheme(scheme) {
            return false;
        }
        match self.admit_session_document_replacement() {
            Ok(_) => true,
            Err(terminal) => {
                self.retain_application_configured_limit(terminal);
                false
            },
        }
    }

    /// Reject an application top-level scheme before JavaScript evaluation, fetch, or admission.
    pub(crate) fn reject_application_session_navigation_scheme(&mut self, scheme: &str) -> bool {
        if self.document_control_profile != DocumentControlProfile::TopLevelSession
            || self.session_navigation_terminal.is_some()
            || matches!(scheme, "http" | "https")
        {
            return false;
        }
        self.retain_application_navigation_failure(
            ApplicationSessionNavigationFailure::UnsupportedScheme {
                scheme: scheme.to_owned(),
            },
        );
        true
    }

    /// Admit one fragment, pushState, or replaceState authority change before mutation.
    pub(crate) fn admit_session_history_change(
        &mut self,
    ) -> Result<HistoryRevision, SessionNavigationTerminal> {
        if let Some(terminal) = self.session_navigation_terminal {
            return Err(terminal);
        }
        let Some(next_revision) = self.history_revision.checked_add(1) else {
            let terminal = SessionNavigationTerminal::CounterOverflow {
                counter: SessionNavigationCounter::HistoryRevision,
            };
            self.session_navigation_terminal = Some(terminal);
            return Err(terminal);
        };
        if next_revision > CONTROLLED_SESSION_MAX_HISTORY_REVISIONS {
            let terminal = SessionNavigationTerminal::HistoryLimitExceeded {
                limit: CONTROLLED_SESSION_MAX_HISTORY_REVISIONS,
                observed: next_revision,
                navigation_id: SessionNavigationId::new(self.session_navigation_id),
                history_revision: HistoryRevision::new(self.history_revision),
            };
            return Err(terminal);
        }
        self.history_revision = next_revision;
        Ok(HistoryRevision::new(next_revision))
    }

    /// Admit an application-initiated same-document change and retain a configured-limit
    /// rejection for the next passive session observation.
    pub(crate) fn admit_application_session_history_change(&mut self) -> bool {
        if self.document_control_profile != DocumentControlProfile::TopLevelSession {
            return false;
        }
        match self.admit_session_history_change() {
            Ok(_) => true,
            Err(terminal) => {
                self.retain_application_configured_limit(terminal);
                false
            },
        }
    }

    fn retain_application_navigation_failure(
        &mut self,
        failure: ApplicationSessionNavigationFailure,
    ) {
        if self.pending_application_navigation_failure.is_some() {
            return;
        }
        self.pending_application_navigation_failure = Some(failure);
    }

    fn retain_application_configured_limit(&mut self, terminal: SessionNavigationTerminal) {
        if matches!(
            terminal,
            SessionNavigationTerminal::DocumentTransitionLimitExceeded { .. }
                | SessionNavigationTerminal::HistoryLimitExceeded { .. }
        ) {
            self.retain_application_navigation_failure(
                ApplicationSessionNavigationFailure::Terminal(terminal),
            );
        }
    }

    /// Retain failure after application admission reserved an identity but pipeline start failed.
    pub(crate) fn note_application_session_navigation_start_failed(&mut self) {
        if self.document_control_profile != DocumentControlProfile::TopLevelSession
            || self.session_navigation_terminal.is_some()
        {
            return;
        }
        self.retain_application_navigation_failure(
            ApplicationSessionNavigationFailure::NavigationStartFailed,
        );
    }

    pub(crate) fn take_application_navigation_failure(
        &mut self,
    ) -> Option<ApplicationSessionNavigationFailure> {
        self.pending_application_navigation_failure.take()
    }

    pub(crate) fn fail_session_redirect_limit(&mut self, observed: u64) {
        if self.document_control_profile != DocumentControlProfile::TopLevelSession
            || self.session_navigation_terminal.is_some()
        {
            return;
        }
        self.session_navigation_terminal = Some(SessionNavigationTerminal::RedirectLimitExceeded {
            limit: embedder_traits::CONTROLLED_SESSION_MAX_REDIRECTS,
            observed,
            navigation_id: SessionNavigationId::new(self.session_navigation_id),
        });
    }

    fn note_session_document_activation(&mut self, replacing_active_document: bool) {
        if self.document_control_profile != DocumentControlProfile::TopLevelSession
            || self.session_navigation_terminal.is_some()
        {
            return;
        }
        let Some(next_document_epoch) = self.document_epoch.checked_add(1) else {
            self.session_navigation_terminal = Some(SessionNavigationTerminal::CounterOverflow {
                counter: SessionNavigationCounter::DocumentEpoch,
            });
            return;
        };
        if replacing_active_document {
            let Some(next_replacements) = self.successful_document_replacements.checked_add(1)
            else {
                self.session_navigation_terminal =
                    Some(SessionNavigationTerminal::CounterOverflow {
                        counter: SessionNavigationCounter::SuccessfulDocumentReplacements,
                    });
                return;
            };
            if next_replacements > CONTROLLED_SESSION_MAX_DOCUMENT_REPLACEMENTS {
                self.session_navigation_terminal =
                    Some(SessionNavigationTerminal::CounterOverflow {
                        counter: SessionNavigationCounter::SuccessfulDocumentReplacements,
                    });
                return;
            }
            self.successful_document_replacements = next_replacements;
        }
        self.document_epoch = next_document_epoch;
    }

    pub(crate) const fn controlled_event_loop_id(&self) -> Option<ScriptEventLoopId> {
        self.controlled_event_loop_id
    }

    pub(crate) const fn document_time_failure(&self) -> Option<DocumentTimeSurface> {
        self.document_time_failure
    }

    pub(crate) fn bind_controlled_event_loop(
        &mut self,
        event_loop_id: ScriptEventLoopId,
        mismatch_surface: DocumentTimeSurface,
    ) -> Result<(), DocumentTimeSurface> {
        if self.document_clock == DocumentClockConfiguration::Realtime {
            return Ok(());
        }
        if let Some(failure) = self.document_time_failure {
            return Err(failure);
        }
        match self.controlled_event_loop_id {
            None => {
                self.controlled_event_loop_id = Some(event_loop_id);
                Ok(())
            },
            Some(bound) if bound == event_loop_id => Ok(()),
            Some(_) => {
                self.document_time_failure = Some(mismatch_surface);
                Err(mismatch_surface)
            },
        }
    }

    pub(crate) fn fail_document_time(&mut self, surface: DocumentTimeSurface) {
        if self.document_clock != DocumentClockConfiguration::Realtime
            && self.document_time_failure.is_none()
        {
            self.document_time_failure = Some(surface);
        }
    }

    /// Set the [`Theme`] on this [`ConstellationWebView`] returning true if the theme changed.
    pub(crate) fn set_theme(&mut self, new_theme: Theme) -> bool {
        let old_theme = std::mem::replace(&mut self.theme, new_theme);
        old_theme != self.theme
    }

    /// Get the [`Theme`] of this [`ConstellationWebView`].
    pub(crate) fn theme(&self) -> Theme {
        self.theme
    }

    pub(crate) fn active_top_level_pipeline(&self) -> Option<(PipelineId, Epoch)> {
        self.active_top_level_pipeline_id
            .map(|pipeline_id| (pipeline_id, self.active_top_level_pipeline_epoch))
    }

    fn advance_navigation_revision_by(
        &mut self,
        amount: u64,
    ) -> Result<u64, NavigationRevisionError> {
        if self.navigation_revision_failure.is_some() {
            return Ok(self.navigation_revision);
        }

        let Some(next_revision) = self.navigation_revision.checked_add(amount) else {
            self.navigation_revision_failure = Some(NavigationRevisionError::Overflow);
            return Ok(self.navigation_revision);
        };
        self.navigation_revision = next_revision;
        Ok(next_revision)
    }

    pub(crate) const fn navigation_revision_failure(&self) -> Option<NavigationRevisionError> {
        self.navigation_revision_failure
    }

    /// Record a completed pipeline-map membership mutation without ever blocking that mutation.
    pub(crate) fn note_pipeline_membership_change(&mut self) {
        if self.pipeline_membership_revision_failure.is_some() {
            return;
        }
        let Some(next) = self.pipeline_membership_revision.checked_add(1) else {
            self.pipeline_membership_revision_failure =
                Some(PipelineMembershipRevisionError::Overflow);
            return;
        };
        self.pipeline_membership_revision = next;
    }

    pub(crate) const fn pipeline_membership_revision(
        &self,
    ) -> (u64, Option<PipelineMembershipRevisionError>) {
        (
            self.pipeline_membership_revision,
            self.pipeline_membership_revision_failure,
        )
    }

    fn is_top_level_change(&self, change: &SessionHistoryChange) -> bool {
        change.browsing_context_id == BrowsingContextId::from(self.webview_id)
    }

    /// Return a canonical navigation snapshot without advancing or otherwise driving the runtime.
    pub(crate) fn top_level_navigation_snapshot(
        &self,
    ) -> Result<TopLevelNavigationSnapshot, NavigationRevisionError> {
        let mut pending_pipeline_ids = self
            .pending_changes
            .iter()
            .filter(|change| self.is_top_level_change(change))
            .map(|change| change.new_pipeline_id)
            .collect::<Vec<_>>();
        pending_pipeline_ids.sort_unstable();
        pending_pipeline_ids.dedup();

        Ok(TopLevelNavigationSnapshot {
            active_pipeline_id: self.active_top_level_pipeline_id,
            active_pipeline_epoch: self.active_top_level_pipeline_epoch,
            navigation_revision: self.navigation_revision,
            pending_pipeline_ids,
        })
    }

    /// Activate a new top-level pipeline and advance the navigation revision atomically.
    pub(crate) fn activate_top_level_pipeline(
        &mut self,
        new_pipeline_id: PipelineId,
    ) -> Result<Option<TopLevelPipelineActivation>, NavigationRevisionError> {
        if self.active_top_level_pipeline_id == Some(new_pipeline_id) {
            return Ok(None);
        }

        self.advance_navigation_revision_by(1)?;
        let old_pipeline_id = self.active_top_level_pipeline_id;
        self.note_session_document_activation(old_pipeline_id.is_some());
        let old_epoch = self.active_top_level_pipeline_epoch;
        let new_epoch = old_epoch.next();
        self.active_top_level_pipeline_id = Some(new_pipeline_id);
        self.active_top_level_pipeline_epoch = new_epoch;

        Ok(Some(TopLevelPipelineActivation {
            old_pipeline_id,
            old_epoch,
            new_epoch,
        }))
    }

    fn target_pipeline_id_for_input_event(
        &self,
        event: &ConstellationInputEvent,
        browsing_contexts: &FxHashMap<BrowsingContextId, BrowsingContext>,
    ) -> Option<PipelineId> {
        if let Some(hit_test_result) = &event.hit_test_result {
            return Some(hit_test_result.pipeline_id);
        }

        // If there's no hit test, send the event to either the hovered or focused browsing context,
        // depending on the event type.
        let browsing_context_id = if matches!(event.event.event, InputEvent::MouseLeftViewport(_)) {
            self.hovered_browsing_context_id
                .unwrap_or(self.focused_browsing_context_id)
        } else {
            self.focused_browsing_context_id
        };

        Some(browsing_contexts.get(&browsing_context_id)?.pipeline_id)
    }

    /// Forward the [`InputEvent`] to this [`ConstellationWebView`]. Returns false if
    /// the event could not be forwarded or true otherwise.
    pub(crate) fn forward_input_event(
        &mut self,
        event: ConstellationInputEvent,
        pipelines: &FxHashMap<PipelineId, Pipeline>,
        browsing_contexts: &FxHashMap<BrowsingContextId, BrowsingContext>,
    ) -> bool {
        let Some(pipeline_id) = self.target_pipeline_id_for_input_event(&event, browsing_contexts)
        else {
            warn!("Unknown pipeline for input event. Ignoring.");
            return false;
        };
        let Some(pipeline) = pipelines.get(&pipeline_id) else {
            warn!("Unknown pipeline id {pipeline_id:?} for input event. Ignoring.");
            return false;
        };

        let mut update_hovered_browsing_context =
            |newly_hovered_browsing_context_id, focus_moving_to_another_iframe: bool| {
                let old_hovered_context_id = std::mem::replace(
                    &mut self.hovered_browsing_context_id,
                    newly_hovered_browsing_context_id,
                );
                if old_hovered_context_id == newly_hovered_browsing_context_id {
                    return;
                }
                let Some(old_hovered_context_id) = old_hovered_context_id else {
                    return;
                };
                let Some(pipeline) = browsing_contexts
                    .get(&old_hovered_context_id)
                    .and_then(|browsing_context| pipelines.get(&browsing_context.pipeline_id))
                else {
                    return;
                };

                let mut synthetic_mouse_leave_event = event.clone();
                synthetic_mouse_leave_event.event.event =
                    InputEvent::MouseLeftViewport(MouseLeftViewportEvent {
                        focus_moving_to_another_iframe,
                    });

                let _ = pipeline
                    .event_loop
                    .send(ScriptThreadMessage::SendInputEvent(
                        self.webview_id,
                        pipeline.id,
                        synthetic_mouse_leave_event,
                    ));
            };

        if let InputEvent::MouseLeftViewport(_) = &event.event.event {
            update_hovered_browsing_context(None, false);
            return true;
        }

        if let InputEvent::MouseMove(_) = &event.event.event {
            update_hovered_browsing_context(Some(pipeline.browsing_context_id), true);
            self.last_mouse_move_point = event
                .hit_test_result
                .as_ref()
                .expect("MouseMove events should always have hit tests.")
                .point_in_viewport;
        }

        let _ = pipeline
            .event_loop
            .send(ScriptThreadMessage::SendInputEvent(
                self.webview_id,
                pipeline.id,
                event,
            ));
        true
    }

    /// If there is an ongoing history traversal request that is waiting on documents to
    /// reload, check to see if none of its pipelines are awaiting activation. If that's the
    /// case unset the ongoing request and return it.
    pub(crate) fn maybe_finish_ongoing_session_history_traversal_request(
        &mut self,
    ) -> Option<SessionHistoryTraversalRequest> {
        let ongoing_history_traversal_request = self.ongoing_history_traversal_request.as_mut()?;

        let pipelines_with_pending_changes = self
            .pending_changes
            .iter()
            .map(|change| change.new_pipeline_id)
            .collect::<FxHashSet<_>>();
        ongoing_history_traversal_request
            .pipelines_awaiting_activation
            .retain(|pipeline_id| pipelines_with_pending_changes.contains(pipeline_id));

        if !ongoing_history_traversal_request
            .pipelines_awaiting_activation
            .is_empty()
        {
            return None;
        }
        Some(
            self.ongoing_history_traversal_request
                .take()
                .expect("Guaranteed above")
                .traversal_request,
        )
    }

    pub(crate) fn has_pending_change(&self) -> bool {
        !self.pending_changes.is_empty()
    }

    pub(crate) fn pending_changes(&self) -> &[SessionHistoryChange] {
        &self.pending_changes
    }

    pub(crate) fn pipeline_is_pending(&self, pipeline_id: PipelineId) -> bool {
        self.pending_changes
            .iter()
            .any(|pending_change| pending_change.new_pipeline_id == pipeline_id)
    }

    pub(crate) fn add_pending_change(
        &mut self,
        change: SessionHistoryChange,
    ) -> Result<(), NavigationRevisionError> {
        debug!(
            "adding pending session history change with {}",
            if change.replace.is_some() {
                "replacement"
            } else {
                "no replacement"
            },
        );
        if self.is_top_level_change(&change) {
            self.advance_navigation_revision_by(1)?;
        }
        self.pending_changes.push(change);
        Ok(())
    }

    pub(crate) fn remove_pending_change_for_pipeline(
        &mut self,
        pipeline_id: PipelineId,
    ) -> Result<Option<SessionHistoryChange>, NavigationRevisionError> {
        let Some(pending_index) = self
            .pending_changes
            .iter()
            .rposition(|change| change.new_pipeline_id == pipeline_id)
        else {
            return Ok(None);
        };
        if self.is_top_level_change(&self.pending_changes[pending_index]) {
            self.advance_navigation_revision_by(1)?;
        }
        Ok(Some(self.pending_changes.swap_remove(pending_index)))
    }

    pub(crate) fn take_pending_changes(
        &mut self,
    ) -> Result<Vec<SessionHistoryChange>, NavigationRevisionError> {
        let top_level_change_count = self
            .pending_changes
            .iter()
            .filter(|change| self.is_top_level_change(change))
            .count() as u64;
        self.advance_navigation_revision_by(top_level_change_count)?;
        Ok(std::mem::take(&mut self.pending_changes))
    }
}

#[cfg(test)]
mod tests {
    use embedder_traits::{CONTROLLED_SESSION_MAX_REDIRECTS, ViewportDetails};
    use servo_base::id::{
        PipelineNamespaceId, ScriptEventLoopId, TEST_BROWSING_CONTEXT_ID,
        TEST_BROWSING_CONTEXT_INDEX, TEST_PIPELINE_INDEX, TEST_SCRIPT_EVENT_LOOP_ID,
        TEST_WEBVIEW_ID,
    };
    use timers::DocumentUnixTime;

    use super::*;
    use crate::session_history::NeedsToReload;

    fn pipeline_id(namespace: u32) -> PipelineId {
        PipelineId {
            namespace_id: PipelineNamespaceId(namespace),
            index: TEST_PIPELINE_INDEX,
        }
    }

    fn child_browsing_context_id(namespace: u32) -> BrowsingContextId {
        BrowsingContextId {
            namespace_id: PipelineNamespaceId(namespace),
            index: TEST_BROWSING_CONTEXT_INDEX,
        }
    }

    fn pending_change(
        pipeline_id: PipelineId,
        browsing_context_id: BrowsingContextId,
        replace: Option<NeedsToReload>,
    ) -> SessionHistoryChange {
        SessionHistoryChange {
            browsing_context_id,
            webview_id: TEST_WEBVIEW_ID,
            new_pipeline_id: pipeline_id,
            replace,
            new_browsing_context_info: None,
            viewport_details: ViewportDetails::default(),
        }
    }

    fn controlled_webview() -> ConstellationWebView {
        ConstellationWebView::new(
            TEST_WEBVIEW_ID,
            TEST_BROWSING_CONTEXT_ID,
            None,
            DocumentClockConfiguration::Controlled {
                initial_time_ns: 7,
                unix_time_origin_ns: DocumentUnixTime::from_nanos(11),
            },
            DocumentControlProfile::SingleDocument,
        )
    }

    fn controlled_session_webview() -> ConstellationWebView {
        ConstellationWebView::new(
            TEST_WEBVIEW_ID,
            TEST_BROWSING_CONTEXT_ID,
            None,
            DocumentClockConfiguration::Controlled {
                initial_time_ns: 7,
                unix_time_origin_ns: DocumentUnixTime::from_nanos(11),
            },
            DocumentControlProfile::TopLevelSession,
        )
    }

    #[test]
    fn controlled_webview_binds_one_event_loop_and_latches_typed_mismatch() {
        let mut webview = controlled_webview();
        assert_eq!(webview.controlled_event_loop_id(), None);
        assert!(
            webview
                .bind_controlled_event_loop(
                    TEST_SCRIPT_EVENT_LOOP_ID,
                    DocumentTimeSurface::CrossEventLoopNavigation,
                )
                .is_ok()
        );
        assert!(
            webview
                .bind_controlled_event_loop(
                    TEST_SCRIPT_EVENT_LOOP_ID,
                    DocumentTimeSurface::CrossEventLoopNavigation,
                )
                .is_ok()
        );

        let other_event_loop = ScriptEventLoopId::new();
        assert_ne!(other_event_loop, TEST_SCRIPT_EVENT_LOOP_ID);
        assert_eq!(
            webview.bind_controlled_event_loop(
                other_event_loop,
                DocumentTimeSurface::CrossEventLoopNavigation,
            ),
            Err(DocumentTimeSurface::CrossEventLoopNavigation)
        );
        assert_eq!(
            webview.document_time_failure(),
            Some(DocumentTimeSurface::CrossEventLoopNavigation)
        );
        assert_eq!(
            webview.controlled_event_loop_id(),
            Some(TEST_SCRIPT_EVENT_LOOP_ID)
        );
        assert_eq!(
            webview.bind_controlled_event_loop(
                TEST_SCRIPT_EVENT_LOOP_ID,
                DocumentTimeSurface::CrossEventLoopIframe,
            ),
            Err(DocumentTimeSurface::CrossEventLoopNavigation)
        );
    }

    #[test]
    fn realtime_webview_does_not_bind_or_latch_document_time_failures() {
        let mut webview = ConstellationWebView::new(
            TEST_WEBVIEW_ID,
            TEST_BROWSING_CONTEXT_ID,
            None,
            DocumentClockConfiguration::Realtime,
            DocumentControlProfile::SingleDocument,
        );
        assert!(
            webview
                .bind_controlled_event_loop(
                    TEST_SCRIPT_EVENT_LOOP_ID,
                    DocumentTimeSurface::CrossEventLoopNavigation,
                )
                .is_ok()
        );
        webview.fail_document_time(DocumentTimeSurface::AuxiliaryWebView);
        assert_eq!(webview.controlled_event_loop_id(), None);
        assert_eq!(webview.document_time_failure(), None);
    }

    #[test]
    fn webview_retains_the_checked_control_profile_independently_from_its_clock() {
        let webview = ConstellationWebView::new(
            TEST_WEBVIEW_ID,
            TEST_BROWSING_CONTEXT_ID,
            None,
            DocumentClockConfiguration::Controlled {
                initial_time_ns: 7,
                unix_time_origin_ns: DocumentUnixTime::from_nanos(11),
            },
            DocumentControlProfile::TopLevelSession,
        );

        assert_eq!(
            webview.document_control_profile(),
            DocumentControlProfile::TopLevelSession,
        );
        assert_eq!(webview.controlled_event_loop_id(), None);
    }

    #[test]
    fn session_identity_starts_at_zero_and_initial_activation_only_advances_document_epoch() {
        let mut webview = controlled_session_webview();
        assert_eq!(
            webview.session_navigation_authority(),
            (
                DocumentEpoch::new(0),
                SessionNavigationId::new(0),
                HistoryRevision::new(0),
                0,
                None,
            )
        );

        assert!(matches!(
            webview.activate_top_level_pipeline(pipeline_id(10)),
            Ok(Some(_))
        ));
        assert_eq!(
            webview.session_navigation_authority().0,
            DocumentEpoch::new(1)
        );
        assert_eq!(
            webview.session_navigation_authority().1,
            SessionNavigationId::new(0)
        );
        assert_eq!(webview.session_navigation_authority().3, 0);
    }

    #[test]
    fn session_replacement_reserves_identity_before_activation() {
        let mut webview = controlled_session_webview();
        assert!(matches!(
            webview.activate_top_level_pipeline(pipeline_id(10)),
            Ok(Some(_))
        ));
        assert_eq!(
            webview.admit_session_document_replacement(),
            Ok(SessionNavigationId::new(1))
        );
        assert_eq!(
            webview.session_navigation_authority().0,
            DocumentEpoch::new(1)
        );
        assert_eq!(
            webview.session_navigation_authority().1,
            SessionNavigationId::new(1)
        );

        assert!(matches!(
            webview.activate_top_level_pipeline(pipeline_id(11)),
            Ok(Some(_))
        ));
        assert_eq!(
            webview.session_navigation_authority().0,
            DocumentEpoch::new(2)
        );
        assert_eq!(webview.session_navigation_authority().3, 1);
    }

    #[test]
    fn session_authority_observation_is_stable_until_checked_admission() {
        let mut webview = controlled_session_webview();
        assert!(matches!(
            webview.activate_top_level_pipeline(pipeline_id(10)),
            Ok(Some(_))
        ));
        let observed = webview.session_navigation_authority();

        assert_eq!(webview.session_navigation_authority(), observed);
        assert_eq!(
            webview.admit_session_history_change(),
            Ok(HistoryRevision::new(1))
        );
        assert_eq!(
            webview.session_navigation_authority(),
            (
                observed.0,
                observed.1,
                HistoryRevision::new(1),
                observed.3,
                None
            )
        );
    }

    #[test]
    fn session_replacement_limit_has_no_state_effect_and_does_not_consume_an_id() {
        let mut webview = controlled_session_webview();
        webview.successful_document_replacements = CONTROLLED_SESSION_MAX_DOCUMENT_REPLACEMENTS;
        let expected = SessionNavigationTerminal::DocumentTransitionLimitExceeded {
            limit: CONTROLLED_SESSION_MAX_DOCUMENT_REPLACEMENTS,
            observed: CONTROLLED_SESSION_MAX_DOCUMENT_REPLACEMENTS + 1,
            next_navigation_id: SessionNavigationId::new(1),
        };

        assert_eq!(webview.admit_session_document_replacement(), Err(expected));
        assert_eq!(webview.admit_session_document_replacement(), Err(expected));
        assert_eq!(
            webview.session_navigation_authority().1,
            SessionNavigationId::new(0)
        );
        assert_eq!(webview.session_navigation_authority().4, None);
    }

    #[test]
    fn session_replacement_limit_admits_max_then_rejects_max_plus_one_without_authority_drift() {
        let mut webview = controlled_session_webview();
        assert!(matches!(
            webview.activate_top_level_pipeline(pipeline_id(10)),
            Ok(Some(_))
        ));
        webview.successful_document_replacements = CONTROLLED_SESSION_MAX_DOCUMENT_REPLACEMENTS - 1;

        assert_eq!(
            webview.admit_session_document_replacement(),
            Ok(SessionNavigationId::new(1))
        );
        assert!(matches!(
            webview.activate_top_level_pipeline(pipeline_id(11)),
            Ok(Some(_))
        ));
        let authority_at_limit = webview.session_navigation_authority();
        assert_eq!(
            authority_at_limit,
            (
                DocumentEpoch::new(2),
                SessionNavigationId::new(1),
                HistoryRevision::new(0),
                CONTROLLED_SESSION_MAX_DOCUMENT_REPLACEMENTS,
                None,
            )
        );

        assert_eq!(
            webview.admit_session_document_replacement(),
            Err(SessionNavigationTerminal::DocumentTransitionLimitExceeded {
                limit: CONTROLLED_SESSION_MAX_DOCUMENT_REPLACEMENTS,
                observed: CONTROLLED_SESSION_MAX_DOCUMENT_REPLACEMENTS + 1,
                next_navigation_id: SessionNavigationId::new(2),
            })
        );
        assert_eq!(webview.session_navigation_authority(), authority_at_limit);
    }

    #[test]
    fn application_replacement_limit_is_observed_once_without_changing_authority() {
        let mut webview = controlled_session_webview();
        webview.successful_document_replacements = CONTROLLED_SESSION_MAX_DOCUMENT_REPLACEMENTS;
        let authority = webview.session_navigation_authority();
        let expected = SessionNavigationTerminal::DocumentTransitionLimitExceeded {
            limit: CONTROLLED_SESSION_MAX_DOCUMENT_REPLACEMENTS,
            observed: CONTROLLED_SESSION_MAX_DOCUMENT_REPLACEMENTS + 1,
            next_navigation_id: SessionNavigationId::new(1),
        };

        assert!(!webview.admit_application_session_document_replacement("https"));
        assert_eq!(webview.session_navigation_authority(), authority);
        assert_eq!(
            webview.take_application_navigation_failure(),
            Some(ApplicationSessionNavigationFailure::Terminal(expected))
        );
        assert_eq!(webview.take_application_navigation_failure(), None);
        assert_eq!(webview.session_navigation_authority(), authority);
    }

    #[test]
    fn application_unsupported_scheme_is_observed_once_before_admission() {
        let mut webview = controlled_session_webview();
        assert!(matches!(
            webview.activate_top_level_pipeline(pipeline_id(10)),
            Ok(Some(_))
        ));
        let authority = webview.session_navigation_authority();

        assert!(webview.reject_application_session_navigation_scheme("file"));
        assert!(webview.reject_application_session_navigation_scheme("data"));
        assert_eq!(webview.session_navigation_authority(), authority);
        assert_eq!(
            webview.take_application_navigation_failure(),
            Some(ApplicationSessionNavigationFailure::UnsupportedScheme {
                scheme: "file".to_owned(),
            })
        );
        assert_eq!(webview.take_application_navigation_failure(), None);
        assert_eq!(webview.session_navigation_authority(), authority);
    }

    #[test]
    fn single_document_profile_does_not_latch_application_scheme_failures() {
        let mut webview = controlled_webview();
        let authority = webview.session_navigation_authority();

        assert!(!webview.reject_application_session_navigation_scheme("file"));
        assert_eq!(webview.take_application_navigation_failure(), None);
        assert_eq!(webview.session_navigation_authority(), authority);
    }

    #[test]
    fn application_start_failure_is_observed_once_after_identity_reservation() {
        let mut webview = controlled_session_webview();
        assert!(matches!(
            webview.activate_top_level_pipeline(pipeline_id(10)),
            Ok(Some(_))
        ));
        let before_admission = webview.session_navigation_authority();

        assert!(webview.admit_application_session_document_replacement("https"));
        let reserved_authority = webview.session_navigation_authority();
        assert_eq!(reserved_authority.0, before_admission.0);
        assert_eq!(reserved_authority.1, SessionNavigationId::new(1));
        assert_eq!(reserved_authority.2, before_admission.2);
        assert_eq!(reserved_authority.3, before_admission.3);
        assert_eq!(reserved_authority.4, None);

        webview.note_application_session_navigation_start_failed();
        assert_eq!(webview.session_navigation_authority(), reserved_authority);
        assert_eq!(
            webview.take_application_navigation_failure(),
            Some(ApplicationSessionNavigationFailure::NavigationStartFailed)
        );
        assert_eq!(webview.take_application_navigation_failure(), None);
        assert_eq!(webview.session_navigation_authority(), reserved_authority);
    }

    #[test]
    fn application_same_document_paths_advance_only_monotonic_history_authority() {
        let mut webview = controlled_session_webview();
        let pipeline_id = pipeline_id(10);
        assert!(matches!(
            webview.activate_top_level_pipeline(pipeline_id),
            Ok(Some(_))
        ));
        let target_before = webview.top_level_navigation_snapshot().unwrap();
        let mut authority_before = webview.session_navigation_authority();

        // These are the three ScriptToConstellation paths which share this admission owner:
        // PushHistoryState, ReplaceHistoryState, and NavigatedToFragment.
        for path in ["pushState", "replaceState", "fragment"] {
            assert!(
                webview.admit_application_session_history_change(),
                "{path} must be admitted below the frozen bound",
            );
            let authority_after = webview.session_navigation_authority();
            assert_eq!(authority_after.0, authority_before.0, "{path}");
            assert_eq!(authority_after.1, authority_before.1, "{path}");
            assert_eq!(
                authority_after.2.get(),
                authority_before.2.get() + 1,
                "{path}",
            );
            assert_eq!(authority_after.3, authority_before.3, "{path}");
            assert_eq!(authority_after.4, None, "{path}");
            assert_eq!(
                webview.top_level_navigation_snapshot().unwrap(),
                target_before,
                "{path} must not rotate PendingTarget navigation authority",
            );
            authority_before = authority_after;
        }
    }

    #[test]
    fn session_history_limit_rejects_without_mutating_authority() {
        let mut webview = controlled_session_webview();
        webview.session_navigation_id = 17;
        webview.history_revision = CONTROLLED_SESSION_MAX_HISTORY_REVISIONS;
        let expected = SessionNavigationTerminal::HistoryLimitExceeded {
            limit: CONTROLLED_SESSION_MAX_HISTORY_REVISIONS,
            observed: CONTROLLED_SESSION_MAX_HISTORY_REVISIONS + 1,
            navigation_id: SessionNavigationId::new(17),
            history_revision: HistoryRevision::new(CONTROLLED_SESSION_MAX_HISTORY_REVISIONS),
        };

        assert_eq!(webview.admit_session_history_change(), Err(expected));
        assert_eq!(webview.admit_session_history_change(), Err(expected));
        assert_eq!(
            webview.session_navigation_authority().2,
            HistoryRevision::new(CONTROLLED_SESSION_MAX_HISTORY_REVISIONS)
        );
        assert_eq!(webview.session_navigation_authority().4, None);
    }

    #[test]
    fn session_history_limit_admits_max_then_rejects_max_plus_one_without_authority_drift() {
        let mut webview = controlled_session_webview();
        webview.session_navigation_id = 17;
        webview.history_revision = CONTROLLED_SESSION_MAX_HISTORY_REVISIONS - 1;

        assert_eq!(
            webview.admit_session_history_change(),
            Ok(HistoryRevision::new(
                CONTROLLED_SESSION_MAX_HISTORY_REVISIONS
            ))
        );
        let authority_at_limit = webview.session_navigation_authority();
        assert_eq!(
            authority_at_limit,
            (
                DocumentEpoch::new(0),
                SessionNavigationId::new(17),
                HistoryRevision::new(CONTROLLED_SESSION_MAX_HISTORY_REVISIONS),
                0,
                None,
            )
        );

        assert_eq!(
            webview.admit_session_history_change(),
            Err(SessionNavigationTerminal::HistoryLimitExceeded {
                limit: CONTROLLED_SESSION_MAX_HISTORY_REVISIONS,
                observed: CONTROLLED_SESSION_MAX_HISTORY_REVISIONS + 1,
                navigation_id: SessionNavigationId::new(17),
                history_revision: HistoryRevision::new(CONTROLLED_SESSION_MAX_HISTORY_REVISIONS),
            })
        );
        assert_eq!(webview.session_navigation_authority(), authority_at_limit);
    }

    #[test]
    fn application_history_limit_is_observed_once_without_changing_authority() {
        let mut webview = controlled_session_webview();
        webview.session_navigation_id = 17;
        webview.history_revision = CONTROLLED_SESSION_MAX_HISTORY_REVISIONS;
        let authority = webview.session_navigation_authority();
        let expected = SessionNavigationTerminal::HistoryLimitExceeded {
            limit: CONTROLLED_SESSION_MAX_HISTORY_REVISIONS,
            observed: CONTROLLED_SESSION_MAX_HISTORY_REVISIONS + 1,
            navigation_id: SessionNavigationId::new(17),
            history_revision: HistoryRevision::new(CONTROLLED_SESSION_MAX_HISTORY_REVISIONS),
        };

        assert!(!webview.admit_application_session_history_change());
        assert_eq!(webview.session_navigation_authority(), authority);
        assert_eq!(
            webview.take_application_navigation_failure(),
            Some(ApplicationSessionNavigationFailure::Terminal(expected))
        );
        assert_eq!(webview.take_application_navigation_failure(), None);
        assert_eq!(webview.session_navigation_authority(), authority);
    }

    #[test]
    fn session_counter_overflow_is_sticky_and_fail_stop() {
        let mut webview = controlled_session_webview();
        webview.session_navigation_id = u64::MAX;
        let expected = SessionNavigationTerminal::CounterOverflow {
            counter: SessionNavigationCounter::NavigationId,
        };

        assert_eq!(webview.admit_session_document_replacement(), Err(expected));
        assert_eq!(webview.admit_session_document_replacement(), Err(expected));
        assert_eq!(
            webview.session_navigation_authority().1,
            SessionNavigationId::new(u64::MAX)
        );
        assert_eq!(webview.session_navigation_authority().4, Some(expected));
    }

    #[test]
    fn session_redirect_limit_is_typed_against_the_reserved_navigation() {
        let mut webview = controlled_session_webview();
        assert_eq!(
            webview.admit_session_document_replacement(),
            Ok(SessionNavigationId::new(1))
        );
        webview.fail_session_redirect_limit(CONTROLLED_SESSION_MAX_REDIRECTS + 1);

        assert_eq!(
            webview.session_navigation_authority().4,
            Some(SessionNavigationTerminal::RedirectLimitExceeded {
                limit: CONTROLLED_SESSION_MAX_REDIRECTS,
                observed: CONTROLLED_SESSION_MAX_REDIRECTS + 1,
                navigation_id: SessionNavigationId::new(1),
            })
        );
    }

    #[test]
    fn single_document_profile_does_not_project_v2_session_identity() {
        let mut webview = controlled_webview();
        assert!(matches!(
            webview.activate_top_level_pipeline(pipeline_id(10)),
            Ok(Some(_))
        ));
        assert_eq!(
            webview.session_navigation_authority(),
            (
                DocumentEpoch::new(0),
                SessionNavigationId::new(0),
                HistoryRevision::new(0),
                0,
                None,
            )
        );
        assert!(webview.permits_session_history_traversal());
    }

    #[test]
    fn top_level_session_rejects_history_traversal_by_profile() {
        let mut session = controlled_session_webview();
        assert!(!session.permits_session_history_traversal());
        assert!(session.reject_session_history_traversal());
        assert_eq!(
            session.document_time_failure(),
            Some(DocumentTimeSurface::HistoryTraversal)
        );

        let mut single_document = controlled_webview();
        assert!(!single_document.reject_session_history_traversal());
        assert_eq!(single_document.document_time_failure(), None);
    }

    #[test]
    fn top_level_navigation_start_advances_revision_and_canonicalizes_pending_ids() {
        let mut webview = controlled_webview();
        let first_pipeline_id = pipeline_id(20);
        let second_pipeline_id = pipeline_id(10);

        assert_eq!(
            webview.add_pending_change(pending_change(
                first_pipeline_id,
                TEST_BROWSING_CONTEXT_ID,
                None,
            )),
            Ok(())
        );
        assert_eq!(
            webview.add_pending_change(pending_change(
                second_pipeline_id,
                TEST_BROWSING_CONTEXT_ID,
                None,
            )),
            Ok(())
        );
        assert_eq!(
            webview.add_pending_change(pending_change(
                first_pipeline_id,
                TEST_BROWSING_CONTEXT_ID,
                None,
            )),
            Ok(())
        );

        let Ok(snapshot) = webview.top_level_navigation_snapshot() else {
            assert!(false, "navigation snapshot unexpectedly failed");
            return;
        };
        assert_eq!(snapshot.navigation_revision, 3);
        assert_eq!(snapshot.active_pipeline_id, None);
        assert_eq!(snapshot.active_pipeline_epoch, Epoch::default());
        assert_eq!(
            snapshot.pending_pipeline_ids,
            vec![second_pipeline_id, first_pipeline_id]
        );
    }

    #[test]
    fn top_level_navigation_cancel_advances_revision_only_for_a_real_removal() {
        let mut webview = controlled_webview();
        let pipeline_id = pipeline_id(10);
        assert_eq!(
            webview
                .add_pending_change(pending_change(pipeline_id, TEST_BROWSING_CONTEXT_ID, None,)),
            Ok(())
        );
        assert!(matches!(
            webview.remove_pending_change_for_pipeline(pipeline_id),
            Ok(Some(_))
        ));
        assert!(matches!(
            webview.remove_pending_change_for_pipeline(pipeline_id),
            Ok(None)
        ));

        let Ok(snapshot) = webview.top_level_navigation_snapshot() else {
            assert!(false, "navigation snapshot unexpectedly failed");
            return;
        };
        assert_eq!(snapshot.navigation_revision, 2);
        assert!(snapshot.pending_pipeline_ids.is_empty());
    }

    #[test]
    fn top_level_redirect_replacement_gets_a_fresh_revision() {
        let mut webview = controlled_webview();
        let redirected_pipeline_id = pipeline_id(10);
        let replacement_pipeline_id = pipeline_id(11);
        assert_eq!(
            webview.add_pending_change(pending_change(
                redirected_pipeline_id,
                TEST_BROWSING_CONTEXT_ID,
                None,
            )),
            Ok(())
        );
        assert!(matches!(
            webview.remove_pending_change_for_pipeline(redirected_pipeline_id),
            Ok(Some(_))
        ));
        assert_eq!(
            webview.add_pending_change(pending_change(
                replacement_pipeline_id,
                TEST_BROWSING_CONTEXT_ID,
                Some(NeedsToReload::No(redirected_pipeline_id)),
            )),
            Ok(())
        );

        let Ok(snapshot) = webview.top_level_navigation_snapshot() else {
            assert!(false, "navigation snapshot unexpectedly failed");
            return;
        };
        assert_eq!(snapshot.navigation_revision, 3);
        assert_eq!(snapshot.pending_pipeline_ids, vec![replacement_pipeline_id]);
    }

    #[test]
    fn top_level_activation_advances_revision_and_active_epoch() {
        let mut webview = controlled_webview();
        let first_pipeline_id = pipeline_id(10);
        let second_pipeline_id = pipeline_id(11);

        assert!(matches!(
            webview.activate_top_level_pipeline(first_pipeline_id),
            Ok(Some(_))
        ));
        assert!(matches!(
            webview.activate_top_level_pipeline(first_pipeline_id),
            Ok(None)
        ));
        assert!(matches!(
            webview.activate_top_level_pipeline(second_pipeline_id),
            Ok(Some(_))
        ));

        let Ok(snapshot) = webview.top_level_navigation_snapshot() else {
            assert!(false, "navigation snapshot unexpectedly failed");
            return;
        };
        assert_eq!(snapshot.navigation_revision, 2);
        assert_eq!(snapshot.active_pipeline_id, Some(second_pipeline_id));
        assert_eq!(snapshot.active_pipeline_epoch, Epoch(2));
    }

    #[test]
    fn child_pending_changes_do_not_affect_top_level_navigation_revision() {
        let mut webview = controlled_webview();
        let child_pipeline_id = pipeline_id(10);
        assert_eq!(
            webview.add_pending_change(pending_change(
                child_pipeline_id,
                child_browsing_context_id(30),
                None,
            )),
            Ok(())
        );

        let Ok(snapshot) = webview.top_level_navigation_snapshot() else {
            assert!(false, "navigation snapshot unexpectedly failed");
            return;
        };
        assert_eq!(snapshot.navigation_revision, 0);
        assert!(snapshot.pending_pipeline_ids.is_empty());
        assert!(matches!(
            webview.remove_pending_change_for_pipeline(child_pipeline_id),
            Ok(Some(_))
        ));

        let Ok(snapshot) = webview.top_level_navigation_snapshot() else {
            assert!(false, "navigation snapshot unexpectedly failed");
            return;
        };
        assert_eq!(snapshot.navigation_revision, 0);
    }

    #[test]
    fn navigation_revision_prevents_aba_snapshot_reuse() {
        let mut webview = controlled_webview();
        let active_pipeline_id = pipeline_id(10);
        let pending_pipeline_id = pipeline_id(11);
        assert!(matches!(
            webview.activate_top_level_pipeline(active_pipeline_id),
            Ok(Some(_))
        ));
        assert_eq!(
            webview.add_pending_change(pending_change(
                pending_pipeline_id,
                TEST_BROWSING_CONTEXT_ID,
                None,
            )),
            Ok(())
        );
        let Ok(before_aba) = webview.top_level_navigation_snapshot() else {
            assert!(false, "navigation snapshot unexpectedly failed");
            return;
        };

        assert!(matches!(
            webview.remove_pending_change_for_pipeline(pending_pipeline_id),
            Ok(Some(_))
        ));
        assert_eq!(
            webview.add_pending_change(pending_change(
                pending_pipeline_id,
                TEST_BROWSING_CONTEXT_ID,
                None,
            )),
            Ok(())
        );
        let Ok(after_aba) = webview.top_level_navigation_snapshot() else {
            assert!(false, "navigation snapshot unexpectedly failed");
            return;
        };

        assert_eq!(before_aba.active_pipeline_id, after_aba.active_pipeline_id);
        assert_eq!(
            before_aba.active_pipeline_epoch,
            after_aba.active_pipeline_epoch
        );
        assert_eq!(
            before_aba.pending_pipeline_ids,
            after_aba.pending_pipeline_ids
        );
        assert_eq!(before_aba.navigation_revision, 2);
        assert_eq!(after_aba.navigation_revision, 4);
    }

    #[test]
    fn navigation_revision_overflow_is_sticky_and_does_not_wrap() {
        let mut webview = controlled_webview();
        webview.navigation_revision = u64::MAX;
        let pipeline_id = pipeline_id(10);

        assert_eq!(
            webview
                .add_pending_change(pending_change(pipeline_id, TEST_BROWSING_CONTEXT_ID, None,)),
            Ok(())
        );
        assert_eq!(webview.navigation_revision, u64::MAX);
        assert!(webview.pipeline_is_pending(pipeline_id));
        assert_eq!(
            webview.navigation_revision_failure(),
            Some(NavigationRevisionError::Overflow)
        );
        assert!(webview.top_level_navigation_snapshot().is_ok());
        assert!(matches!(
            webview.activate_top_level_pipeline(pipeline_id),
            Ok(Some(_))
        ));
        assert_eq!(
            webview.active_top_level_pipeline().map(|(id, _)| id),
            Some(pipeline_id)
        );
    }

    #[test]
    fn pipeline_membership_revision_latches_without_blocking_later_mutations() {
        let mut webview = controlled_webview();
        webview.pipeline_membership_revision = u64::MAX;

        webview.note_pipeline_membership_change();
        webview.note_pipeline_membership_change();

        assert_eq!(
            webview.pipeline_membership_revision(),
            (u64::MAX, Some(PipelineMembershipRevisionError::Overflow))
        );
    }
}

/// A [`HistoryTraversalRequest`] that is in progress because it is waiting
/// for documents that need reloading.
pub(crate) struct OngoingHistoryTraversalRequest {
    /// The [`HistoryTraversalRequest`] that spawned this series of navigations.
    pub traversal_request: SessionHistoryTraversalRequest,
    /// The ids of all the `Pipeline`s that needed reloading for this traversal.
    /// Multiple pipelines can be traversed if the top-level document contained
    /// `<iframe>`s / browsing contexts. The traversal is only done when all of
    /// the pipelines are ready or have failed to load.
    pub pipelines_awaiting_activation: FxHashSet<PipelineId>,
}
