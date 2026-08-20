/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::time::Duration as StdDuration;

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::codegen::GenericBindings::UserActivationBinding::UserActivationMethods;
use script_bindings::reflector::{Reflector, reflect_dom_object_with_cx};
use servo_base::cross_process_instant::CrossProcessInstant;
use time::Duration as TimeDuration;
use timers::{DocumentClock, DocumentTime};

use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::document::{
    Document, SameOriginDescendantNavigablesIterator, SameoriginAncestorNavigablesIterator,
};
use crate::dom::globalscope::GlobalScope;

/// <https://html.spec.whatwg.org/multipage/#the-useractivation-interface>
#[dom_struct]
pub(crate) struct UserActivation {
    reflector_: Reflector,
}

impl UserActivation {
    fn new_inherited() -> UserActivation {
        UserActivation {
            reflector_: Reflector::new(),
        }
    }

    pub(crate) fn new(cx: &mut JSContext, global: &GlobalScope) -> DomRoot<UserActivation> {
        reflect_dom_object_with_cx(Box::new(UserActivation::new_inherited()), global, cx)
    }

    /// <https://html.spec.whatwg.org/multipage/#activation-notification>
    pub(crate) fn handle_user_activation_notification(document: &Document) {
        // Step 1.
        // > Assert: document is fully active.
        debug_assert!(
            document.is_fully_active(),
            "Document should be fully active at this moment"
        );

        // Step 2.
        // > Let windows be « document's relevant global object ».
        let owner_window = document.window();
        rooted_vec!(let mut windows <- vec![Dom::from_ref(owner_window)].into_iter());

        // Step 3.
        // > Extend windows with the active window of each of document's ancestor navigables.
        // TODO: this would not work for disimilar origin ancestor, since we don't store the document in this script thread.
        for document in SameoriginAncestorNavigablesIterator::new(DomRoot::from_ref(document)) {
            windows.push(Dom::from_ref(document.window()));
        }

        // Step 4.
        // > Extend windows with the active window of each of document's descendant navigables, filtered to include only
        // > those navigables whose active document's origin is same origin with document's origin.
        for document in SameOriginDescendantNavigablesIterator::new(document) {
            windows.push(Dom::from_ref(document.window()));
        }

        // Step 5.
        // > For each window in windows:
        let current_timestamp = UserActivationTimestamp::current(
            &owner_window.as_global_scope().document_clock(),
        );
        for window in windows.iter() {
            // Step 5.1.
            // > Set window's last activation timestamp to the current high resolution time.
            window.set_last_activation_timestamp(current_timestamp);

            // Step 5.2.
            // > Notify the close watcher manager about user activation given window.
            // TODO: impl close watcher
        }
    }
}

impl UserActivationMethods<crate::DomTypeHolder> for UserActivation {
    /// <https://html.spec.whatwg.org/multipage/#dom-useractivation-hasbeenactive>
    fn HasBeenActive(&self) -> bool {
        // > The hasBeenActive getter steps are to return true if this's relevant global object has sticky activation, and false otherwise.
        self.global().as_window().has_sticky_activation()
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-useractivation-isactive>
    fn IsActive(&self) -> bool {
        // > The isActive getter steps are to return true if this's relevant global object has transient activation, and false otherwise.
        self.global().as_window().has_transient_activation()
    }
}

/// Timestamp definition specific to [`UserActivation`].
/// > ... which is either a DOMHighResTimeStamp, positive infinity (indicating that W has never been activated), or negative infinity
/// > (indicating that the activation has been consumed). Initially positive infinity.
/// > <https://html.spec.whatwg.org/multipage/#user-activation-data-model>
#[derive(Clone, Copy, Debug, PartialEq, MallocSizeOf)]
pub(crate) enum UserActivationTime {
    /// A host monotonic timestamp used by normal interactive Servo.
    Host(CrossProcessInstant),
    /// A timestamp in the controlled Window's shared document-clock domain.
    Document(DocumentTime),
}

impl UserActivationTime {
    fn current(clock: &DocumentClock) -> Self {
        Self::current_with_host(clock, CrossProcessInstant::now)
    }

    fn current_with_host(
        clock: &DocumentClock,
        host_now: impl FnOnce() -> CrossProcessInstant,
    ) -> Self {
        if clock.is_controlled() {
            Self::Document(clock.now())
        } else {
            Self::Host(host_now())
        }
    }

    fn is_at_or_after(self, earlier: Self) -> bool {
        match (self, earlier) {
            (Self::Host(current), Self::Host(earlier)) => current >= earlier,
            (Self::Document(current), Self::Document(earlier)) => current >= earlier,
            // Comparing clock domains would make host time observable in Controlled mode.
            _ => false,
        }
    }

    fn is_before_expiry(self, activation: Self, duration_ms: i64) -> bool {
        match (self, activation) {
            (Self::Host(current), Self::Host(activation)) => {
                current < activation + TimeDuration::milliseconds(duration_ms)
            },
            (Self::Document(current), Self::Document(activation)) => {
                let Ok(duration_ms) = u64::try_from(duration_ms) else {
                    return false;
                };
                let Ok(expiry) = activation.checked_add(StdDuration::from_millis(duration_ms))
                else {
                    // If the positive expiry is beyond DocumentTime's representable range, every
                    // representable current timestamp after activation is still transient.
                    return current >= activation;
                };
                current < expiry
            },
            // A timestamp from another domain cannot establish transient activation.
            _ => false,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, MallocSizeOf)]
pub(crate) enum UserActivationTimestamp {
    NegativeInfinity,
    TimeStamp(UserActivationTime),
    #[default]
    PositiveInfinity,
}

impl UserActivationTimestamp {
    pub(crate) fn current(clock: &DocumentClock) -> Self {
        Self::TimeStamp(UserActivationTime::current(clock))
    }

    fn current_with_host(
        clock: &DocumentClock,
        host_now: impl FnOnce() -> CrossProcessInstant,
    ) -> Self {
        Self::TimeStamp(UserActivationTime::current_with_host(clock, host_now))
    }

    pub(crate) fn has_sticky_activation(self, clock: &DocumentClock) -> bool {
        self.has_sticky_activation_with_host(clock, CrossProcessInstant::now)
    }

    fn has_sticky_activation_with_host(
        self,
        clock: &DocumentClock,
        host_now: impl FnOnce() -> CrossProcessInstant,
    ) -> bool {
        match self {
            Self::NegativeInfinity => true,
            Self::TimeStamp(activation) => {
                UserActivationTime::current_with_host(clock, host_now)
                    .is_at_or_after(activation)
            },
            Self::PositiveInfinity => false,
        }
    }

    pub(crate) fn has_transient_activation(
        self,
        clock: &DocumentClock,
        duration_ms: i64,
    ) -> bool {
        self.has_transient_activation_with_host(clock, duration_ms, CrossProcessInstant::now)
    }

    fn has_transient_activation_with_host(
        self,
        clock: &DocumentClock,
        duration_ms: i64,
        host_now: impl FnOnce() -> CrossProcessInstant,
    ) -> bool {
        match self {
            Self::TimeStamp(activation) => {
                let current = UserActivationTime::current_with_host(clock, host_now);
                current.is_at_or_after(activation) &&
                    current.is_before_expiry(activation, duration_ms)
            },
            Self::NegativeInfinity | Self::PositiveInfinity => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use timers::{DocumentClockConfiguration, DocumentUnixTime};

    use super::*;

    fn controlled_clock(initial_time_ns: u128) -> DocumentClock {
        DocumentClock::new(DocumentClockConfiguration::Controlled {
            initial_time_ns,
            unix_time_origin_ns: DocumentUnixTime::default(),
        })
    }

    #[test]
    fn activation_is_captured_at_virtual_time() {
        let clock = controlled_clock(42_000_000);

        assert_eq!(
            UserActivationTimestamp::current(&clock),
            UserActivationTimestamp::TimeStamp(UserActivationTime::Document(
                DocumentTime::from_nanos(42_000_000),
            )),
        );
    }

    #[test]
    fn sticky_activation_persists_after_virtual_advance() {
        let clock = controlled_clock(10_000_000);
        let activation = UserActivationTimestamp::current(&clock);

        clock
            .advance_to(DocumentTime::from_nanos(60_000_000_000))
            .unwrap();

        assert!(activation.has_sticky_activation(&clock));
    }

    #[test]
    fn transient_activation_expires_on_virtual_deadline() {
        let clock = controlled_clock(10_000_000);
        let activation = UserActivationTimestamp::current(&clock);

        clock
            .advance_to(DocumentTime::from_nanos(5_009_999_999))
            .unwrap();
        assert!(activation.has_transient_activation(&clock, 5_000));

        clock
            .advance_to(DocumentTime::from_nanos(5_010_000_000))
            .unwrap();
        assert!(!activation.has_transient_activation(&clock, 5_000));
    }

    #[test]
    fn controlled_activation_never_samples_host_time() {
        let clock = controlled_clock(1);
        let activation = UserActivationTimestamp::current_with_host(&clock, || {
            panic!("controlled activation sampled the host clock")
        });

        assert!(activation.has_sticky_activation_with_host(&clock, || {
            panic!("controlled sticky activation sampled the host clock")
        }));
        assert!(activation.has_transient_activation_with_host(&clock, 5_000, || {
            panic!("controlled transient activation sampled the host clock")
        }));
    }
}
