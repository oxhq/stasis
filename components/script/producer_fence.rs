/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Script-event-loop adapters for document producer fences.

use std::fmt;
use std::sync::{Arc, Mutex};

use net_traits::image_cache::{
    ImageCacheResponseCallback, ImageCacheResponseMessage, ImageResponse,
};
use net_traits::{BoxedFetchCallback, FetchResponseMsg};
use timers::{DocumentProducerFence, DocumentProducerGuard, DocumentProducerKind};

use crate::tasks::task::TaskBox;

/// Owns one response-stream producer until EOF completes normally.
///
/// Losing the callback before EOF is not a successful resource completion: the networking API may
/// still have an unresolved promise or request state. In that case the guard latches a producer
/// terminal while consuming the lease, so an empty snapshot cannot be mistaken for quiescence.
struct FetchStreamProducer {
    guard: Option<DocumentProducerGuard>,
}

impl FetchStreamProducer {
    fn new(guard: DocumentProducerGuard) -> Self {
        Self { guard: Some(guard) }
    }

    fn is_live(&self) -> bool {
        self.guard.is_some()
    }

    fn complete(&mut self) {
        drop(self.guard.take());
    }

    fn abandon(&mut self) {
        if let Some(guard) = self.guard.take() {
            // This guard was created by and remains owned by this fence adapter. An unknown lease
            // would be an internal invariant failure, but abandonment is also used during unwind,
            // where panicking again would abort the process. The sticky terminal is authoritative
            // on every valid path.
            let _ = guard.abandon();
        }
    }
}

impl Drop for FetchStreamProducer {
    fn drop(&mut self) {
        self.abandon();
    }
}

/// Resolves the response-stream producer after one callback returns.
///
/// Keeping this as a local call guard matters because a caller may catch a callback panic and
/// retain the outer `BoxedFetchCallback`; the producer must become terminal at the unwind boundary
/// rather than wait for that outer callback to be dropped.
struct FetchCallbackCompletion<'a> {
    producer: &'a mut FetchStreamProducer,
    terminal: bool,
}

impl Drop for FetchCallbackCompletion<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.producer.abandon();
        } else if self.terminal {
            self.producer.complete();
        }
    }
}

/// A local event-loop message that keeps its producer live through message handling.
pub(crate) struct DocumentProducerEnvelope<T> {
    pub(crate) message: T,
    guard: Option<DocumentProducerGuard>,
}

impl<T: fmt::Debug> fmt::Debug for DocumentProducerEnvelope<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DocumentProducerEnvelope")
            .field("message", &self.message)
            .field("producer_guarded", &self.guard.is_some())
            .finish()
    }
}

impl<T> DocumentProducerEnvelope<T> {
    pub(crate) fn new(message: T, guard: Option<DocumentProducerGuard>) -> Self {
        Self { message, guard }
    }

    pub(crate) fn into_parts(self) -> (T, Option<DocumentProducerGuard>) {
        (self.message, self.guard)
    }
}

/// Keep a resource ticket live until the terminal callback has queued its event-loop work.
pub(crate) fn fence_fetch_callback(
    fence: &DocumentProducerFence,
    callback: BoxedFetchCallback,
    is_terminal: impl Fn(&FetchResponseMsg) -> bool + Send + 'static,
) -> Result<BoxedFetchCallback, timers::DocumentProducerFenceError> {
    fence_fetch_callback_with_admission(
        || fence.begin(DocumentProducerKind::Resource),
        callback,
        is_terminal,
    )
}

fn fence_fetch_callback_with_admission(
    admit: impl FnOnce() -> Result<DocumentProducerGuard, timers::DocumentProducerFenceError>,
    mut callback: BoxedFetchCallback,
    is_terminal: impl Fn(&FetchResponseMsg) -> bool + Send + 'static,
) -> Result<BoxedFetchCallback, timers::DocumentProducerFenceError> {
    let mut producer = FetchStreamProducer::new(admit()?);
    Ok(Box::new(move |message| {
        if !producer.is_live() {
            return;
        }
        let terminal = is_terminal(&message);
        let completion = FetchCallbackCompletion {
            producer: &mut producer,
            terminal,
        };
        callback(message);
        drop(completion);
    }))
}

/// Keep a resource ticket live through a complete Fetch response stream.
pub(crate) fn fence_fetch_until_eof(
    fence: &DocumentProducerFence,
    callback: BoxedFetchCallback,
) -> Result<BoxedFetchCallback, timers::DocumentProducerFenceError> {
    fence_fetch_callback(fence, callback, |message| {
        matches!(message, FetchResponseMsg::ProcessResponseEOF(..))
    })
}

/// Keep an image ticket live through its terminal callback's event-loop enqueue.
pub(crate) fn fence_image_callback(
    fence: &DocumentProducerFence,
    callback: impl Fn(DocumentProducerEnvelope<ImageCacheResponseMessage>) + Send + 'static,
) -> ImageCacheResponseCallback {
    let fence = fence.clone();
    let guard = Arc::new(Mutex::new(Some(
        fence
            .begin(DocumentProducerKind::Image)
            .expect("document image producer sequence exhausted"),
    )));
    Box::new(move |message| {
        let terminal = match &message {
            ImageCacheResponseMessage::VectorImageRasterizationComplete(..) => true,
            ImageCacheResponseMessage::NotifyPendingImageLoadStatus(response) => matches!(
                &response.response,
                ImageResponse::Loaded(..) | ImageResponse::FailedToLoadOrDecode
            ),
        };
        let message_guard = {
            let mut guard = guard
                .lock()
                .expect("document image producer guard poisoned");
            if guard.is_none() {
                return;
            }
            if terminal {
                guard.take()
            } else {
                Some(
                    fence
                        .begin(DocumentProducerKind::Image)
                        .expect("document image message sequence exhausted"),
                )
            }
        };
        callback(DocumentProducerEnvelope::new(message, message_guard));
    })
}

/// Keep a task producer live until its boxed task has either run or been discarded.
pub(crate) struct ProducerFencedTaskBox {
    inner: Box<dyn TaskBox>,
    guard: DocumentProducerGuard,
}

impl ProducerFencedTaskBox {
    pub(crate) fn new(inner: Box<dyn TaskBox>, guard: DocumentProducerGuard) -> Self {
        Self { inner, guard }
    }

    fn run_inner(self, run: impl FnOnce(Box<dyn TaskBox>)) {
        let Self { inner, guard } = self;
        run(inner);
        drop(guard);
    }
}

impl TaskBox for ProducerFencedTaskBox {
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    fn run_box(self: Box<Self>, cx: &mut js::context::JSContext) {
        (*self).run_inner(|inner| inner.run_box(cx));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    use net_traits::request::RequestId;
    use net_traits::{ResourceFetchTiming, ResourceTimingType};
    use timers::{
        DocumentProducerCheckpoint, DocumentProducerObservation, DocumentProducerObserver,
    };

    use super::*;

    #[test]
    fn resource_completion_is_recorded_after_the_terminal_callback() {
        let fence = DocumentProducerFence::default();
        let callback_saw_pending = Arc::new(Mutex::new(Vec::new()));
        let observations = callback_saw_pending.clone();
        let callback_fence = fence.clone();
        let mut callback = fence_fetch_until_eof(
            &fence,
            Box::new(move |_| {
                observations
                    .lock()
                    .unwrap()
                    .push(callback_fence.snapshot().pending());
            }),
        )
        .unwrap();

        assert_eq!(fence.snapshot().pending(), 1);
        callback(FetchResponseMsg::ProcessRequestBody(RequestId::default()));
        assert_eq!(fence.snapshot().pending(), 1);
        callback(FetchResponseMsg::ProcessResponseEOF(
            RequestId::default(),
            Ok(()),
            ResourceFetchTiming::new(ResourceTimingType::Resource),
        ));

        assert_eq!(*callback_saw_pending.lock().unwrap(), vec![1, 1]);
        let complete = fence.snapshot();
        assert!(complete.is_empty());
        assert_eq!(complete.terminal_error(), None);
        assert_eq!(
            complete
                .for_kind(DocumentProducerKind::Resource)
                .completed(),
            1
        );
    }

    #[test]
    fn dropping_a_callback_before_eof_completes_and_latches_abandonment() {
        let fence = DocumentProducerFence::default();
        let callback = fence_fetch_until_eof(&fence, Box::new(|_| {})).unwrap();
        assert_eq!(fence.snapshot().pending(), 1);

        drop(callback);

        let abandoned = fence.snapshot();
        assert!(abandoned.is_empty());
        assert!(matches!(
            abandoned.terminal_error(),
            Some(timers::DocumentProducerFenceError::ProducerAbandoned(lease_id))
                if lease_id.kind() == DocumentProducerKind::Resource
        ));
        assert_eq!(
            abandoned
                .for_kind(DocumentProducerKind::Resource)
                .completed(),
            1
        );
    }

    #[test]
    fn panicking_fetch_callback_releases_its_ticket_before_the_outer_callback_is_dropped() {
        let fence = DocumentProducerFence::default();
        let mut callback = fence_fetch_until_eof(
            &fence,
            Box::new(|_| panic!("synthetic fetch callback panic")),
        )
        .unwrap();

        let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            callback(FetchResponseMsg::ProcessRequestBody(RequestId::default()));
        }));

        assert!(unwind.is_err());
        assert!(fence.snapshot().is_empty());
        assert!(matches!(
            fence.snapshot().terminal_error(),
            Some(timers::DocumentProducerFenceError::ProducerAbandoned(lease_id))
                if lease_id.kind() == DocumentProducerKind::Resource
        ));
        assert_eq!(
            fence
                .snapshot()
                .for_kind(DocumentProducerKind::Resource)
                .completed(),
            1
        );
        drop(callback);
    }

    struct DropProbe(Arc<AtomicBool>);

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn failed_resource_admission_is_typed_and_discards_the_raw_callback() {
        let callback_dropped = Arc::new(AtomicBool::new(false));
        let probe = DropProbe(callback_dropped.clone());
        let result = fence_fetch_callback_with_admission(
            || Err(timers::DocumentProducerFenceError::CounterOverflow),
            Box::new(move |_| {
                let _keep_probe_alive = &probe;
                panic!("an unadmitted callback must never run");
            }),
            |_| false,
        );

        assert!(matches!(
            result,
            Err(timers::DocumentProducerFenceError::CounterOverflow)
        ));
        assert!(callback_dropped.load(Ordering::SeqCst));
    }

    #[test]
    fn dropping_an_abandoned_image_listener_completes_its_ticket() {
        let fence = DocumentProducerFence::default();
        let callback = fence_image_callback(&fence, |_| {});
        assert_eq!(
            fence
                .snapshot()
                .for_kind(DocumentProducerKind::Image)
                .pending(),
            1
        );

        drop(callback);

        let complete = fence.snapshot();
        assert!(complete.is_empty());
        assert_eq!(
            complete.for_kind(DocumentProducerKind::Image).completed(),
            1
        );
    }

    fn queued_message_cannot_look_empty_after_its_producer_is_dropped(kind: DocumentProducerKind) {
        let fence = DocumentProducerFence::default();
        let producer_guard = fence.begin(kind).unwrap();
        let envelope =
            DocumentProducerEnvelope::new("queued message", Some(fence.begin(kind).unwrap()));
        drop(producer_guard);
        let mut observer = DocumentProducerObserver::default();
        let first = DocumentProducerCheckpoint::ZERO.checked_next().unwrap();
        let second = first.checked_next().unwrap();
        let third = second.checked_next().unwrap();
        let fourth = third.checked_next().unwrap();

        assert!(matches!(
            observer.observe(&fence, first),
            Ok(DocumentProducerObservation::Busy(_))
        ));
        assert!(matches!(
            observer.observe(&fence, second),
            Ok(DocumentProducerObservation::Busy(_))
        ));

        let (message, producer_guard) = envelope.into_parts();
        assert_eq!(message, "queued message");
        drop(producer_guard);

        assert!(matches!(
            observer.observe(&fence, third),
            Ok(DocumentProducerObservation::FirstEmpty(_))
        ));
        assert!(matches!(
            observer.observe(&fence, fourth),
            Ok(DocumentProducerObservation::StableEmpty(_))
        ));
    }

    #[test]
    fn queued_resource_message_retains_its_lease_after_callback_drop() {
        queued_message_cannot_look_empty_after_its_producer_is_dropped(
            DocumentProducerKind::Resource,
        );
    }

    #[test]
    fn queued_image_message_retains_its_lease_after_listener_drop() {
        queued_message_cannot_look_empty_after_its_producer_is_dropped(DocumentProducerKind::Image);
    }

    struct NamedTask;

    impl TaskBox for NamedTask {
        fn name(&self) -> &'static str {
            "named producer task"
        }

        fn run_box(self: Box<Self>, _cx: &mut js::context::JSContext) {}
    }

    #[test]
    fn discarding_a_fenced_task_completes_its_ticket() {
        let fence = DocumentProducerFence::default();
        let guard = fence.begin(DocumentProducerKind::Task).unwrap();
        let task = ProducerFencedTaskBox::new(Box::new(NamedTask), guard);

        assert_eq!(task.name(), "named producer task");
        assert_eq!(fence.snapshot().pending(), 1);
        drop(task);
        assert!(fence.snapshot().is_empty());
    }

    #[test]
    fn running_a_fenced_task_keeps_its_ticket_until_the_inner_task_returns() {
        let fence = DocumentProducerFence::default();
        let guard = fence.begin(DocumentProducerKind::Task).unwrap();
        let task = ProducerFencedTaskBox::new(Box::new(NamedTask), guard);
        let observed_fence = fence.clone();

        task.run_inner(move |_| assert_eq!(observed_fence.snapshot().pending(), 1));

        assert!(fence.snapshot().is_empty());
    }

    #[test]
    fn panicking_inner_task_still_completes_its_ticket_during_unwind() {
        let fence = DocumentProducerFence::default();
        let guard = fence.begin(DocumentProducerKind::Task).unwrap();
        let task = ProducerFencedTaskBox::new(Box::new(NamedTask), guard);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            task.run_inner(|_| panic!("inner task panic"));
        }));

        assert!(result.is_err());
        assert!(fence.snapshot().is_empty());
        assert_eq!(
            fence
                .snapshot()
                .for_kind(DocumentProducerKind::Task)
                .completed(),
            1
        );
    }
}
