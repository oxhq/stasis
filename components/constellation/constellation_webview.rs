/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::collections::VecDeque;

use embedder_traits::user_contents::UserContentManagerId;
use embedder_traits::{DocumentClockConfiguration, InputEvent, MouseLeftViewportEvent, Theme};
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
    ) -> Self {
        Self {
            webview_id,
            user_content_manager_id,
            document_clock,
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
        if self.document_clock != DocumentClockConfiguration::Realtime &&
            self.document_time_failure.is_none()
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
    use embedder_traits::ViewportDetails;
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
            webview.add_pending_change(pending_change(
                pipeline_id,
                TEST_BROWSING_CONTEXT_ID,
                None,
            )),
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
        assert_eq!(
            snapshot.pending_pipeline_ids,
            vec![replacement_pipeline_id]
        );
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
            webview.add_pending_change(pending_change(
                pipeline_id,
                TEST_BROWSING_CONTEXT_ID,
                None,
            )),
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
            (
                u64::MAX,
                Some(PipelineMembershipRevisionError::Overflow)
            )
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
