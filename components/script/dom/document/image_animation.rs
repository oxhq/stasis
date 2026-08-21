/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::Cell;
use std::ffi::c_void;
use std::sync::Arc;
use std::time::Duration;

use embedder_traits::UntrustedNodeAddress;
use layout_api::{AnimatingImages, ImageAnimationTimelineError};
use paint_api::ImageUpdate;
use parking_lot::{Mutex, RwLock};
use pixels::Repeat;
use rustc_hash::FxHashMap;
use script_bindings::codegen::GenericBindings::WindowBinding::WindowMethods;
use script_bindings::root::Dom;
use smallvec::SmallVec;
use style::dom::OpaqueNode;
use timers::{TimerControlError, TimerEventRequest, TimerId};
use webrender_api::ImageKey;

use crate::dom::bindings::refcounted::Trusted;
use crate::dom::bindings::trace::NoTrace;
use crate::dom::from_untrusted_node_address;
use crate::dom::node::Node;
use crate::dom::window::Window;
use crate::event_loop::script_thread::with_script_thread;

/// Owner-captured facts needed to classify retained image animations without sampling time.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ImageAnimationObservationContext {
    /// Whether this document's image-animation timeline is bound to controlled document time.
    pub(crate) timeline_controlled: bool,
    /// Exact [`Document`](super::Document) fully-active state captured by its owner.
    ///
    /// This must not be derived from rendering throttling: throttling does not pause animated
    /// image callbacks.
    pub(crate) document_fully_active: bool,
}

/// A mechanical classification of one image retained by the animation manager.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RetainedImageAnimationClass {
    /// An unfinished animation with an exact finite loop bound.
    Finite,
    /// An unfinished multi-frame animation which loops forever.
    ///
    /// Settlement policy can treat this class as open-ended; the manager only reports the
    /// decoded loop mechanics.
    Infinite,
    /// A completed, single-frame, or document-inactive image which cannot currently advance.
    Inert,
    /// An active multi-frame image whose exact loop bound or progress is unavailable.
    UnsupportedLoopCount,
    /// An active multi-frame image whose timeline is not controlled by document time.
    UnsupportedTimeline,
}

/// Stable process-local identity and classification for one retained animated image.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RetainedImageAnimationObservation {
    /// Node identity used as the key in [`AnimatingImages`].
    pub(crate) node: OpaqueNode,
    /// WebRender image identity, when the retained raster has been registered.
    ///
    /// `node` remains the manager identity when registration has not assigned a key yet.
    pub(crate) image_key: Option<ImageKey>,
    /// Policy-free lifecycle classification captured from retained state.
    pub(crate) class: RetainedImageAnimationClass,
}

/// Exact counts derived from a canonical retained-image inventory.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ImageAnimationPendingCounts {
    pub(crate) retained: u64,
    pub(crate) finite: u64,
    pub(crate) infinite: u64,
    pub(crate) inert: u64,
    pub(crate) unsupported_loop_count: u64,
    pub(crate) unsupported_timeline: u64,
}

impl ImageAnimationPendingCounts {
    fn record(&mut self, class: RetainedImageAnimationClass) {
        self.retained = checked_image_count_increment(self.retained);
        let count = match class {
            RetainedImageAnimationClass::Finite => &mut self.finite,
            RetainedImageAnimationClass::Infinite => &mut self.infinite,
            RetainedImageAnimationClass::Inert => &mut self.inert,
            RetainedImageAnimationClass::UnsupportedLoopCount => &mut self.unsupported_loop_count,
            RetainedImageAnimationClass::UnsupportedTimeline => &mut self.unsupported_timeline,
        };
        *count = checked_image_count_increment(*count);
    }
}

fn checked_image_count_increment(count: u64) -> u64 {
    count
        .checked_add(1)
        .expect("an in-memory animated-image inventory cannot exceed u64::MAX entries")
}

/// A policy-free image-animation snapshot captured from manager-owned state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ImageAnimationPendingObservation {
    /// Canonical retained-image inventory, ordered by manager node identity.
    pub(crate) retained: Vec<RetainedImageAnimationObservation>,
    /// Exact class counts derived from `retained`.
    pub(crate) counts: ImageAnimationPendingCounts,
    /// Callback identity retained by the manager, before an outer-scheduler membership join.
    pub(crate) retained_callback_timer_id: Option<TimerId>,
    /// First checked scheduler failure retained by the manager.
    pub(crate) scheduler_terminal: Option<TimerControlError>,
    /// First checked image-timeline failure retained by the manager in controlled mode.
    pub(crate) timeline_terminal: Option<ImageAnimationTimelineError>,
}

fn classify_retained_image(
    frame_count: usize,
    loop_count: Option<&Repeat>,
    completed_loops: Option<u32>,
    context: ImageAnimationObservationContext,
) -> RetainedImageAnimationClass {
    if frame_count <= 1 || finite_animation_is_complete(loop_count, completed_loops) {
        return RetainedImageAnimationClass::Inert;
    }

    // Document-inactive images remain inert even when metadata which would be needed to resume
    // them is not controlled. Target activity authority must be revalidated before settling from
    // an observation containing this classification.
    if !context.document_fully_active {
        return RetainedImageAnimationClass::Inert;
    }

    if loop_count.is_none()
        || matches!(loop_count, Some(Repeat::Finite(_))) && completed_loops.is_none()
    {
        return RetainedImageAnimationClass::UnsupportedLoopCount;
    }

    if !context.timeline_controlled {
        return RetainedImageAnimationClass::UnsupportedTimeline;
    }

    match loop_count {
        Some(Repeat::Infinite) => RetainedImageAnimationClass::Infinite,
        Some(Repeat::Finite(_)) => RetainedImageAnimationClass::Finite,
        None => unreachable!("missing loop metadata was classified above"),
    }
}

fn finite_animation_is_complete(loop_count: Option<&Repeat>, completed_loops: Option<u32>) -> bool {
    let (Some(Repeat::Finite(maximum_loops)), Some(completed_loops)) =
        (loop_count, completed_loops)
    else {
        return false;
    };
    completed_loops >= maximum_loops.get()
}

#[derive(Clone, Default, JSTraceable, MallocSizeOf)]
#[cfg_attr(crown, crown::unrooted_must_root_lint::must_root)]
pub struct ImageAnimationManager {
    /// The set of [`AnimatingImages`] which is used to communicate the addition
    /// and removal of animating images from layout.
    #[no_trace]
    #[conditional_malloc_size_of]
    animating_images: Arc<RwLock<AnimatingImages>>,

    /// The [`TimerId`] of the currently scheduled animated image update callback.
    #[no_trace]
    callback_timer_id: Cell<Option<TimerId>>,

    /// The first checked scheduler failure observed while arranging the next image frame.
    ///
    /// This remains sticky so pending-work inspection can distinguish a failed scheduling
    /// attempt from an animation that naturally has no next frame.
    #[no_trace]
    #[ignore_malloc_size_of = "TimerControlError has no heap allocations"]
    timer_control_error: Cell<Option<TimerControlError>>,

    /// The first checked image-timeline failure observed in controlled mode.
    ///
    /// Realtime mode preserves its historical fail-loud behavior instead of converting a host
    /// timeline failure into controlled pending-state evidence.
    #[no_trace]
    timeline_error: Cell<Option<ImageAnimationTimelineError>>,

    /// A map of nodes with in-progress image animations. This is kept outside
    /// of [`Self::animating_images`] as that data structure is shared with layout.
    rooted_nodes: FxHashMap<NoTrace<OpaqueNode>, Dom<Node>>,
}

impl ImageAnimationManager {
    pub(crate) fn animating_images(&self) -> Arc<RwLock<AnimatingImages>> {
        self.animating_images.clone()
    }

    pub(crate) fn timer_control_error(&self) -> Option<TimerControlError> {
        self.timer_control_error.get()
    }

    pub(crate) fn timeline_error(&self) -> Option<ImageAnimationTimelineError> {
        self.timeline_error.get()
    }

    fn mark_scheduled_callback_ready(&self, expected_timer_id: TimerId) -> bool {
        if self.callback_timer_id.get() == Some(expected_timer_id) {
            self.callback_timer_id.set(None);
            return true;
        }
        false
    }

    /// Capture retained image-animation state without sampling the timeline or scheduler.
    ///
    /// The returned timer ID is intentionally not converted into a deadline here. The pending
    /// snapshot owner must join that identity to the same outer-scheduler observation used for
    /// guarded advancement. Likewise, `context` must be captured by the document owner rather
    /// than inferred from callback presence: a callback can already be ready while its retained
    /// ID is awaiting owner-side reconciliation.
    pub(crate) fn pending_observation(
        &self,
        context: ImageAnimationObservationContext,
    ) -> ImageAnimationPendingObservation {
        let images = self.animating_images.read();
        let mut retained = images
            .node_to_state_map
            .iter()
            .map(|(node, state)| RetainedImageAnimationObservation {
                node: *node,
                image_key: state.image_key(),
                class: classify_retained_image(
                    state.image.frames.len(),
                    state.image.loop_count.as_ref(),
                    state.completed_loops,
                    context,
                ),
            })
            .collect::<Vec<_>>();
        retained.sort_unstable_by_key(|observation| observation.node.id());

        let mut counts = ImageAnimationPendingCounts::default();
        for observation in &retained {
            counts.record(observation.class);
        }

        ImageAnimationPendingObservation {
            retained,
            counts,
            retained_callback_timer_id: self.callback_timer_id.get(),
            scheduler_terminal: self.timer_control_error.get(),
            timeline_terminal: self.timeline_error.get(),
        }
    }

    fn duration_to_next_frame(
        &self,
        now: f64,
    ) -> Result<Option<Duration>, ImageAnimationTimelineError> {
        let images = self.animating_images.read();
        let mut nodes = images.node_to_state_map.keys().copied().collect::<Vec<_>>();
        nodes.sort_unstable_by_key(OpaqueNode::id);
        let mut next_frame = None;
        for node in nodes {
            let state = images
                .node_to_state_map
                .get(&node)
                .expect("a retained animated-image node must remain present under the read lock");
            let Some(duration) = state.duration_to_next_frame(now)? else {
                continue;
            };
            next_frame = Some(next_frame.map_or(duration, |next: Duration| next.min(duration)));
        }
        Ok(next_frame)
    }

    pub(crate) fn update_active_frames(&self, window: &Window, now: f64) {
        if self.animating_images.read().is_empty() {
            return;
        }
        if self.timer_control_error.get().is_some() || self.timeline_error.get().is_some() {
            return;
        }

        let mut timeline_error = None;
        let mut updates = SmallVec::<[ImageUpdate; 1]>::new();
        {
            let mut images = self.animating_images.write();
            let mut nodes = images.node_to_state_map.keys().copied().collect::<Vec<_>>();
            nodes.sort_unstable_by_key(OpaqueNode::id);
            for node in nodes {
                let state = images.node_to_state_map.get_mut(&node).expect(
                    "a retained animated-image node must remain present under the write lock",
                );
                match state.update_frame_for_animation_timeline_value(now) {
                    Ok(false) => continue,
                    Ok(true) => {},
                    Err(error) => {
                        timeline_error.get_or_insert(error);
                        break;
                    },
                }

                let image = &state.image;
                let frame = image
                    .frame_data(state.active_frame)
                    .expect("No frame found")
                    .clone();
                if let Some(mut descriptor) =
                    image.webrender_image_descriptor_and_offset_for_frame()
                {
                    descriptor.offset = frame.byte_range.start as i32;
                    updates.push(ImageUpdate::UpdateImageForAnimation(
                        image.id.unwrap(),
                        descriptor,
                    ));
                } else {
                    error!("Doing normal image update which will be slow!");
                }
            }
        }
        window
            .paint_api()
            .update_images(window.webview_id().into(), updates);

        if let Some(error) = timeline_error {
            self.handle_timeline_error(window, error);
            return;
        }

        self.maybe_schedule_update(window, now);
    }

    /// This does three things:
    ///  - Root any nodes with newly animating images
    ///  - Schedule an image update for newly animating images
    ///  - Cancel animations for any nodes that no longer have layout boxes.
    pub(crate) fn do_post_reflow_update(&mut self, window: &Window, now: f64) {
        // Cancel animations for any images that are no longer rendering.
        self.rooted_nodes.retain(|opaque_node, node| {
            if node.is_being_rendered(None) {
                return true;
            }
            self.animating_images.write().remove(opaque_node.0);
            false
        });

        if self.animating_images().write().clear_dirty() {
            self.root_nodes_with_newly_animating_images();
            self.maybe_schedule_update(window, now);
        }
    }

    fn root_nodes_with_newly_animating_images(&mut self) {
        for opaque_node in self.animating_images().read().node_to_state_map.keys() {
            #[expect(unsafe_code)]
            self.rooted_nodes
                .entry(NoTrace(*opaque_node))
                .or_insert_with(|| {
                    // SAFETY: This should be safe as this method is run directly after layout,
                    // which should not remove any nodes.
                    let address = UntrustedNodeAddress(opaque_node.0 as *const c_void);
                    unsafe { Dom::from_ref(&*from_untrusted_node_address(address)) }
                });
        }
    }

    fn maybe_schedule_update(&self, window: &Window, now: f64) {
        if self.timer_control_error.get().is_some() || self.timeline_error.get().is_some() {
            return;
        }

        let duration = match self.duration_to_next_frame(now) {
            Ok(duration) => duration,
            Err(error) => {
                self.handle_timeline_error(window, error);
                return;
            },
        };

        with_script_thread(|script_thread| {
            if let Some(current_timer_id) = self.callback_timer_id.take() {
                script_thread.cancel_timer(current_timer_id);
            }

            if let Some(duration) = duration {
                let trusted_window = Trusted::new(window);
                let callback_timer_id = Arc::new(Mutex::new(None));
                let callback_timer_id_for_dispatch = callback_timer_id.clone();
                let result = script_thread.try_schedule_timer(TimerEventRequest {
                    callback: Box::new(move || {
                        let window = trusted_window.root();
                        let document = window.Document();
                        if let Some(timer_id) = *callback_timer_id_for_dispatch.lock() {
                            let is_current = document
                                .image_animation_manager()
                                .mark_scheduled_callback_ready(timer_id);
                            if is_current {
                                document.set_has_pending_animated_image_update();
                            }
                        }
                    }),
                    duration,
                });
                if let Ok(timer_id) = result {
                    *callback_timer_id.lock() = Some(timer_id);
                }
                self.record_schedule_result(
                    result,
                    window.as_global_scope().document_clock().is_controlled(),
                );
            }
        })
    }

    fn record_schedule_result(&self, result: Result<TimerId, TimerControlError>, controlled: bool) {
        match result {
            Ok(timer_id) => self.callback_timer_id.set(Some(timer_id)),
            Err(error) if !controlled => {
                panic!("realtime animated image scheduling failed: {error}")
            },
            Err(error) => {
                debug!("Not scheduling animated image update: {error}");
                if self.timer_control_error.get().is_none() {
                    self.timer_control_error.set(Some(error));
                }
                self.callback_timer_id.set(None);
            },
        }
    }

    fn handle_timeline_error(&self, window: &Window, error: ImageAnimationTimelineError) {
        if !window.as_global_scope().document_clock().is_controlled() {
            panic!("realtime animated image timeline failed: {error:?}");
        }

        debug!("Not advancing controlled animated image timeline: {error:?}");
        with_script_thread(|script_thread| {
            if let Some(timer_id) = self.callback_timer_id.take() {
                script_thread.cancel_timer(timer_id);
            }
        });
        self.record_controlled_timeline_error(error);
    }

    fn record_controlled_timeline_error(&self, error: ImageAnimationTimelineError) {
        if self.timeline_error.get().is_none() {
            self.timeline_error.set(Some(error));
        }
        self.callback_timer_id.set(None);
    }

    pub(crate) fn cancel_animations_for_node(&mut self, node: &Node) {
        let opaque_node = node.to_opaque();
        self.animating_images().write().remove(opaque_node);
        self.rooted_nodes.remove(&NoTrace(opaque_node));
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use layout_api::ImageAnimationState;
    use pixels::{CorsStatus, ImageFrame, ImageMetadata, PixelFormat, RasterImage};
    use timers::{DocumentClockError, TimerScheduler};

    use super::*;

    const CONTROLLED_RUNNING: ImageAnimationObservationContext = ImageAnimationObservationContext {
        timeline_controlled: true,
        document_fully_active: true,
    };

    fn repeat(count: u32) -> Repeat {
        Repeat::Finite(NonZeroU32::new(count).unwrap())
    }

    fn raster_image(loop_count: Repeat, frame_count: usize) -> Arc<RasterImage> {
        let frames = std::iter::repeat_with(|| ImageFrame {
            delay: Some(Duration::from_millis(100)),
            byte_range: 0..1,
            width: 1,
            height: 1,
        })
        .take(frame_count)
        .collect();
        Arc::new(RasterImage {
            metadata: ImageMetadata {
                width: 1,
                height: 1,
            },
            format: PixelFormat::BGRA8,
            id: None,
            cors_status: CorsStatus::Unsafe,
            bytes: Arc::new(vec![0]),
            frames,
            is_opaque: false,
            loop_count: Some(loop_count),
        })
    }

    #[test]
    fn pure_classification_distinguishes_finite_infinite_and_unsupported_work() {
        assert_eq!(
            classify_retained_image(2, Some(&Repeat::Infinite), None, CONTROLLED_RUNNING),
            RetainedImageAnimationClass::Infinite,
        );
        assert_eq!(
            classify_retained_image(2, Some(&repeat(2)), Some(1), CONTROLLED_RUNNING),
            RetainedImageAnimationClass::Finite,
        );
        assert_eq!(
            classify_retained_image(2, None, None, CONTROLLED_RUNNING),
            RetainedImageAnimationClass::UnsupportedLoopCount,
        );
        assert_eq!(
            classify_retained_image(
                2,
                Some(&Repeat::Infinite),
                None,
                ImageAnimationObservationContext {
                    timeline_controlled: false,
                    document_fully_active: true,
                },
            ),
            RetainedImageAnimationClass::UnsupportedTimeline,
        );
    }

    #[test]
    fn completed_single_frame_and_document_inactive_images_remain_inert() {
        assert_eq!(
            classify_retained_image(2, Some(&repeat(2)), Some(2), CONTROLLED_RUNNING),
            RetainedImageAnimationClass::Inert,
        );
        assert_eq!(
            classify_retained_image(1, Some(&Repeat::Infinite), None, CONTROLLED_RUNNING),
            RetainedImageAnimationClass::Inert,
        );
        assert_eq!(
            classify_retained_image(
                2,
                Some(&Repeat::Infinite),
                None,
                ImageAnimationObservationContext {
                    timeline_controlled: false,
                    document_fully_active: false,
                },
            ),
            RetainedImageAnimationClass::Inert,
        );
    }

    #[test]
    fn pending_observation_is_canonical_and_counts_retained_classes() {
        let manager = ImageAnimationManager::default();
        let mut finished = ImageAnimationState::new(raster_image(repeat(1), 2), 0.0);
        finished.completed_loops = Some(1);
        {
            let mut images = manager.animating_images.write();
            images.node_to_state_map.insert(
                OpaqueNode(9),
                ImageAnimationState::new(raster_image(Repeat::Infinite, 2), 0.0),
            );
            images.node_to_state_map.insert(OpaqueNode(3), finished);
        }

        let observation = manager.pending_observation(CONTROLLED_RUNNING);

        assert_eq!(
            observation
                .retained
                .iter()
                .map(|image| image.node.id())
                .collect::<Vec<_>>(),
            vec![3, 9],
        );
        assert_eq!(
            observation.counts,
            ImageAnimationPendingCounts {
                retained: 2,
                finite: 0,
                infinite: 1,
                inert: 1,
                unsupported_loop_count: 0,
                unsupported_timeline: 0,
            },
        );
        assert_eq!(observation.retained_callback_timer_id, None);
        assert_eq!(observation.scheduler_terminal, None);
        assert_eq!(observation.timeline_terminal, None);
    }

    #[test]
    fn pending_observation_retains_exact_callback_identity() {
        let manager = ImageAnimationManager::default();
        let mut scheduler = TimerScheduler::default();
        let timer_id = scheduler.schedule_timer(TimerEventRequest {
            callback: Box::new(|| {}),
            duration: Duration::from_secs(1),
        });
        manager.record_schedule_result(Ok(timer_id), true);

        let observation = manager.pending_observation(CONTROLLED_RUNNING);

        assert_eq!(observation.retained_callback_timer_id, Some(timer_id));
        assert_eq!(observation.scheduler_terminal, None);
        assert_eq!(observation.timeline_terminal, None);

        let replacement_timer_id = scheduler.schedule_timer(TimerEventRequest {
            callback: Box::new(|| {}),
            duration: Duration::from_secs(2),
        });
        manager.record_schedule_result(Ok(replacement_timer_id), true);

        assert!(!manager.mark_scheduled_callback_ready(timer_id));
        assert_eq!(
            manager
                .pending_observation(CONTROLLED_RUNNING)
                .retained_callback_timer_id,
            Some(replacement_timer_id),
        );

        assert!(manager.mark_scheduled_callback_ready(replacement_timer_id));
        assert_eq!(
            manager
                .pending_observation(CONTROLLED_RUNNING)
                .retained_callback_timer_id,
            None,
        );
    }

    #[test]
    fn first_controlled_scheduler_failure_is_sticky() {
        let manager = ImageAnimationManager::default();
        let error = TimerControlError::Clock(DocumentClockError::Overflow);

        manager.record_schedule_result(Err(error), true);
        manager.record_schedule_result(Err(TimerControlError::SequenceExhausted), true);

        assert_eq!(manager.callback_timer_id.get(), None);
        assert_eq!(manager.timer_control_error(), Some(error));
        assert_eq!(manager.timeline_error(), None);
        assert_eq!(
            manager
                .pending_observation(CONTROLLED_RUNNING)
                .scheduler_terminal,
            Some(error),
        );
    }

    #[test]
    #[should_panic(expected = "realtime animated image scheduling failed")]
    fn realtime_scheduler_failure_remains_fail_loud() {
        ImageAnimationManager::default()
            .record_schedule_result(Err(TimerControlError::DeadlineOverflow), false);
    }

    #[test]
    fn first_controlled_timeline_overflow_is_sticky() {
        let manager = ImageAnimationManager::default();
        manager
            .record_controlled_timeline_error(ImageAnimationTimelineError::TimelineValueOutOfRange);
        manager.record_controlled_timeline_error(
            ImageAnimationTimelineError::CompletedLoopCountOverflow,
        );

        assert_eq!(
            manager.timeline_error(),
            Some(ImageAnimationTimelineError::TimelineValueOutOfRange),
        );
        assert_eq!(
            manager
                .pending_observation(CONTROLLED_RUNNING)
                .timeline_terminal,
            Some(ImageAnimationTimelineError::TimelineValueOutOfRange),
        );
        assert_eq!(manager.callback_timer_id.get(), None);
        assert_eq!(manager.timer_control_error(), None);
    }
}
