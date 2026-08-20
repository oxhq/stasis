/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use servo::EventLoopWaker;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WakeGeneration {
    servo: u64,
    protocol: u64,
}

impl WakeGeneration {
    pub fn servo_changed_since(self, earlier: Self) -> bool {
        self.servo != earlier.servo
    }
}

#[derive(Default)]
struct WakeState {
    generation: Mutex<WakeGeneration>,
    changed: Condvar,
}

#[derive(Clone, Default)]
pub struct ShellWaker(Arc<WakeState>);

impl ShellWaker {
    pub fn snapshot(&self) -> WakeGeneration {
        *self.0.generation.lock().expect("wake mutex poisoned")
    }

    pub fn notify_protocol_input(&self) {
        let mut generation = self.0.generation.lock().expect("wake mutex poisoned");
        generation.protocol = generation
            .protocol
            .checked_add(1)
            .expect("protocol wake generation exhausted");
        self.0.changed.notify_all();
    }

    pub fn wait_for_change(
        &self,
        observed: WakeGeneration,
        deadline: Instant,
    ) -> Result<WakeGeneration, WaitError> {
        let mut generation = self.0.generation.lock().expect("wake mutex poisoned");
        loop {
            if *generation != observed {
                return Ok(*generation);
            }

            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return Err(WaitError::DeadlineExceeded);
            };
            let (next_generation, timeout) = self
                .0
                .changed
                .wait_timeout(generation, remaining.max(Duration::from_nanos(1)))
                .expect("wake mutex poisoned while waiting");
            generation = next_generation;
            if timeout.timed_out() && *generation == observed {
                return Err(WaitError::DeadlineExceeded);
            }
        }
    }
}

impl EventLoopWaker for ShellWaker {
    fn clone_box(&self) -> Box<dyn EventLoopWaker> {
        Box::new(self.clone())
    }

    fn wake(&self) {
        let mut generation = self.0.generation.lock().expect("wake mutex poisoned");
        generation.servo = generation
            .servo
            .checked_add(1)
            .expect("Servo wake generation exhausted");
        self.0.changed.notify_all();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WaitError {
    DeadlineExceeded,
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn protocol_and_servo_wakes_have_distinct_generations() {
        let waker = ShellWaker::default();
        let initial = waker.snapshot();
        waker.notify_protocol_input();
        let after_protocol = waker.snapshot();
        waker.wake();
        let after_servo = waker.snapshot();

        assert_ne!(initial, after_protocol);
        assert_ne!(after_protocol, after_servo);
        assert!(!after_protocol.servo_changed_since(initial));
        assert!(after_servo.servo_changed_since(after_protocol));
    }
}
