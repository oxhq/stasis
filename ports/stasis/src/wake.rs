/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use servo::EventLoopWaker;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WakeGeneration {
    servo: u128,
    control_response: u128,
    protocol: u128,
}

impl WakeGeneration {
    pub fn servo_changed_since(self, earlier: Self) -> bool {
        self.servo != earlier.servo
    }

    pub fn control_response_changed_since(self, earlier: Self) -> bool {
        self.control_response != earlier.control_response
    }

    pub fn protocol_changed_since(self, earlier: Self) -> bool {
        self.protocol != earlier.protocol
    }

    fn checked_next(self, source: WakeSource) -> Option<Self> {
        let mut next = self;
        match source {
            WakeSource::Servo => next.servo = next.servo.checked_add(1)?,
            WakeSource::ControlResponse => {
                next.control_response = next.control_response.checked_add(1)?
            },
            WakeSource::ProtocolInput => next.protocol = next.protocol.checked_add(1)?,
        }
        Some(next)
    }
}

#[derive(Default)]
struct GuardedWakeState {
    generation: WakeGeneration,
    exhaustion: Option<WakeGenerationExhaustion>,
}

#[derive(Default)]
struct WakeState {
    guarded: Mutex<GuardedWakeState>,
    changed: Condvar,
    #[cfg(test)]
    about_to_wait: AtomicBool,
}

#[derive(Clone, Default)]
pub struct ShellWaker(Arc<WakeState>);

impl ShellWaker {
    pub fn snapshot(&self) -> WakeGeneration {
        self.0
            .guarded
            .lock()
            .expect("wake mutex poisoned")
            .generation
    }

    /// Snapshot the wake domain while preserving a previously latched exhaustion.
    pub fn snapshot_checked(&self) -> Result<WakeGeneration, WakeGenerationExhaustion> {
        let state = self.0.guarded.lock().expect("wake mutex poisoned");
        match state.exhaustion {
            Some(exhaustion) => Err(exhaustion),
            None => Ok(state.generation),
        }
    }

    /// Wake a host waiting for a completed control response without classifying that response as
    /// new Servo work.
    pub fn notify_control_response(&self) {
        self.advance_generation(WakeSource::ControlResponse);
    }

    pub fn notify_protocol_input(&self) {
        self.advance_generation(WakeSource::ProtocolInput);
    }

    pub fn wait_for_change(
        &self,
        observed: WakeGeneration,
        deadline: Instant,
    ) -> Result<WakeGeneration, WaitError> {
        self.wait_for_change_checked(observed, deadline)
            .map_err(|_| WaitError::DeadlineExceeded)
    }

    /// Wait for a generation change while preserving typed generation-exhaustion evidence.
    ///
    /// New controlled-runtime callers should use this method. [`Self::wait_for_change`] remains
    /// as a compatibility surface for the baseline shell loop.
    pub fn wait_for_change_checked(
        &self,
        observed: WakeGeneration,
        deadline: Instant,
    ) -> Result<WakeGeneration, WakeWaitError> {
        let mut state = self.0.guarded.lock().expect("wake mutex poisoned");
        loop {
            if let Some(exhaustion) = state.exhaustion {
                return Err(WakeWaitError::GenerationExhausted(exhaustion));
            }
            if state.generation != observed {
                return Ok(state.generation);
            }

            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return Err(WakeWaitError::DeadlineExceeded);
            };
            #[cfg(test)]
            self.0.about_to_wait.store(true, Ordering::SeqCst);
            let (next_state, _) = self
                .0
                .changed
                .wait_timeout(state, remaining.max(Duration::from_nanos(1)))
                .expect("wake mutex poisoned while waiting");
            #[cfg(test)]
            self.0.about_to_wait.store(false, Ordering::SeqCst);
            state = next_state;
        }
    }

    fn advance_generation(&self, source: WakeSource) {
        let mut state = self.0.guarded.lock().expect("wake mutex poisoned");
        if state.exhaustion.is_some() {
            self.0.changed.notify_all();
            return;
        }
        match state.generation.checked_next(source) {
            Some(generation) => state.generation = generation,
            None => {
                state.exhaustion = Some(WakeGenerationExhaustion {
                    source,
                    generation: state.generation,
                });
            },
        }
        self.0.changed.notify_all();
    }
}

impl EventLoopWaker for ShellWaker {
    fn clone_box(&self) -> Box<dyn EventLoopWaker> {
        Box::new(self.clone())
    }

    fn wake(&self) {
        self.advance_generation(WakeSource::Servo);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WakeSource {
    Servo,
    ControlResponse,
    ProtocolInput,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WakeGenerationExhaustion {
    pub source: WakeSource,
    pub generation: WakeGeneration,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WaitError {
    DeadlineExceeded,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WakeWaitError {
    DeadlineExceeded,
    GenerationExhausted(WakeGenerationExhaustion),
}

#[cfg(test)]
mod tests {
    use std::thread;

    use super::*;

    fn wait_until_waiter_reaches_condvar(waker: &ShellWaker) {
        let deadline = Instant::now() + Duration::from_secs(1);
        while !waker.0.about_to_wait.load(Ordering::SeqCst) {
            assert!(
                Instant::now() < deadline,
                "waiter did not reach the condition-variable boundary"
            );
            thread::yield_now();
        }
    }

    fn trigger(waker: &ShellWaker, source: WakeSource) {
        match source {
            WakeSource::Servo => waker.wake(),
            WakeSource::ControlResponse => waker.notify_control_response(),
            WakeSource::ProtocolInput => waker.notify_protocol_input(),
        }
    }

    #[test]
    fn a_wake_before_wait_is_not_lost() {
        let waker = ShellWaker::default();
        let observed = waker.snapshot();
        waker.wake();

        let current = waker
            .wait_for_change(observed, Instant::now() + Duration::from_secs(1))
            .unwrap();

        assert_ne!(current, observed);
    }

    #[test]
    fn wake_sources_have_distinct_generations() {
        let waker = ShellWaker::default();
        let initial = waker.snapshot();

        waker.notify_control_response();
        let after_control = waker.snapshot();
        waker.notify_protocol_input();
        let after_protocol = waker.snapshot();
        waker.wake();
        let after_servo = waker.snapshot();

        assert!(after_control.control_response_changed_since(initial));
        assert!(!after_control.protocol_changed_since(initial));
        assert!(!after_control.servo_changed_since(initial));
        assert!(after_protocol.protocol_changed_since(after_control));
        assert!(!after_protocol.servo_changed_since(after_control));
        assert!(!after_protocol.control_response_changed_since(after_control));
        assert!(after_servo.servo_changed_since(after_protocol));
        assert!(!after_servo.control_response_changed_since(after_protocol));
        assert!(!after_servo.protocol_changed_since(after_protocol));
    }

    #[test]
    fn wake_at_the_condvar_boundary_is_not_lost() {
        let waker = ShellWaker::default();
        let observed = waker.snapshot();
        let waiter_waker = waker.clone();
        let waiter = thread::spawn(move || {
            waiter_waker.wait_for_change_checked(observed, Instant::now() + Duration::from_secs(1))
        });

        wait_until_waiter_reaches_condvar(&waker);
        waker.wake();

        assert_eq!(
            waiter.join().expect("waiter should not panic"),
            Ok(waker.snapshot())
        );
    }

    #[test]
    fn spurious_notification_does_not_look_like_a_wake() {
        let waker = ShellWaker::default();
        let observed = waker.snapshot();
        let waiter_waker = waker.clone();
        let waiter = thread::spawn(move || {
            waiter_waker
                .wait_for_change_checked(observed, Instant::now() + Duration::from_millis(100))
        });

        wait_until_waiter_reaches_condvar(&waker);
        let state = waker.0.guarded.lock().expect("wake mutex poisoned");
        waker.0.changed.notify_all();
        drop(state);

        assert_eq!(
            waiter.join().expect("waiter should not panic"),
            Err(WakeWaitError::DeadlineExceeded)
        );
        assert_eq!(waker.snapshot(), observed);
    }

    #[test]
    fn every_generation_exhaustion_is_typed_and_never_wraps() {
        for source in [
            WakeSource::Servo,
            WakeSource::ControlResponse,
            WakeSource::ProtocolInput,
        ] {
            let waker = ShellWaker::default();
            let mut exhausted = WakeGeneration::default();
            match source {
                WakeSource::Servo => exhausted.servo = u128::MAX,
                WakeSource::ControlResponse => exhausted.control_response = u128::MAX,
                WakeSource::ProtocolInput => exhausted.protocol = u128::MAX,
            }
            waker
                .0
                .guarded
                .lock()
                .expect("wake mutex poisoned")
                .generation = exhausted;

            trigger(&waker, source);

            let expected = WakeGenerationExhaustion {
                source,
                generation: exhausted,
            };
            assert_eq!(
                waker.wait_for_change_checked(exhausted, Instant::now()),
                Err(WakeWaitError::GenerationExhausted(expected))
            );
            assert_eq!(waker.snapshot_checked(), Err(expected));
            assert_eq!(waker.snapshot(), exhausted);

            // Once any source is exhausted, the wake domain freezes rather than allowing another
            // source to create a snapshot which can no longer be interpreted safely.
            trigger(&waker, WakeSource::ProtocolInput);
            assert_eq!(waker.snapshot(), exhausted);

            // The compatibility API also returns immediately and does not retain the old panic.
            assert_eq!(
                waker.wait_for_change(exhausted, Instant::now()),
                Err(WaitError::DeadlineExceeded)
            );
        }
    }
}
