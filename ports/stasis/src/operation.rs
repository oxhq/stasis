/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Nonblocking ownership of one same-build document-control response.
//!
//! `DocumentControlReceiver` is consuming: every receive attempt either returns the receiver
//! again or produces its sole terminal result. Keeping that ownership rule here prevents the
//! shell owner loop from accidentally dropping a response while it services protocol input or
//! Servo wakes.

use std::time::{Duration, Instant};

use servo::document_control::{
    DocumentControlOutcome, DocumentControlReceiveOutcome, DocumentControlReceiver,
    DocumentControlTryReceiveOutcome,
};

/// Whether a completed transport result is authoritative or may have crossed a page-state
/// mutation boundary without returning its observation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlOutcomeDisposition {
    /// The requested mechanical action completed with an authoritative post-action observation.
    Completed,
    /// The command was rejected before mutation, or a read-only command failed in transport.
    DefinitiveFailure,
    /// A turn, guarded timer activation, or native mutation may have committed and must never be
    /// retried.
    Indeterminate,
}

/// The sole terminal result of a pending control operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControlOperationCompletion {
    disposition: ControlOutcomeDisposition,
    outcome: DocumentControlReceiveOutcome,
}

impl ControlOperationCompletion {
    fn new(outcome: DocumentControlReceiveOutcome) -> Self {
        Self {
            disposition: disposition_for(&outcome),
            outcome,
        }
    }

    /// Return the mutation certainty of this terminal result.
    pub const fn disposition(&self) -> ControlOutcomeDisposition {
        self.disposition
    }

    /// Consume this wrapper and return the exact result expected by the settlement coordinator.
    pub fn into_receive_outcome(self) -> DocumentControlReceiveOutcome {
        self.outcome
    }
}

/// Result of polling one owner-thread control operation.
pub enum ControlOperationPoll {
    /// No response is ready and the consuming receiver remains armed.
    Pending(PendingControlOperation),
    /// The receiver produced its only terminal result.
    Complete(ControlOperationCompletion),
}

/// One in-flight document-control response retained by the shell owner thread.
pub struct PendingControlOperation {
    receiver: DocumentControlReceiver,
    deadline: Instant,
}

impl PendingControlOperation {
    /// Retain a response receiver until the supplied checked wall deadline.
    pub const fn new(receiver: DocumentControlReceiver, deadline: Instant) -> Self {
        Self { receiver, deadline }
    }

    /// Return the wall deadline without consulting or advancing document time.
    pub const fn deadline(&self) -> Instant {
        self.deadline
    }

    /// Poll without blocking.
    ///
    /// At or beyond the wall deadline, a zero-duration receive still accepts a response which is
    /// already ready. Otherwise it applies the receiver's command-aware timeout semantics: an
    /// Observe failure is definitive, while a DriveOneTurn or AdvanceTo failure is indeterminate.
    pub fn poll(self, now: Instant) -> ControlOperationPoll {
        if now >= self.deadline {
            let outcome = self.receiver.recv_timeout(Duration::ZERO);
            return ControlOperationPoll::Complete(ControlOperationCompletion::new(outcome));
        }

        match self.receiver.try_recv() {
            DocumentControlTryReceiveOutcome::Pending(receiver) => {
                ControlOperationPoll::Pending(Self {
                    receiver,
                    deadline: self.deadline,
                })
            },
            DocumentControlTryReceiveOutcome::Complete(outcome) => {
                ControlOperationPoll::Complete(ControlOperationCompletion::new(outcome))
            },
        }
    }

    /// Abandon the response explicitly. This never promises to roll back page work.
    pub fn cancel(self) -> ControlOperationCompletion {
        ControlOperationCompletion::new(self.receiver.cancel())
    }
}

fn disposition_for(outcome: &DocumentControlReceiveOutcome) -> ControlOutcomeDisposition {
    match outcome {
        DocumentControlReceiveOutcome::CommandOutcome(
            DocumentControlOutcome::Completed(_) |
            DocumentControlOutcome::AutomationCompleted { .. },
        ) => ControlOutcomeDisposition::Completed,
        DocumentControlReceiveOutcome::CommandOutcome(DocumentControlOutcome::Rejected(_)) |
        DocumentControlReceiveOutcome::ObserveTransportFailure(_) |
        DocumentControlReceiveOutcome::AutomationTransportFailure(_) => {
            ControlOutcomeDisposition::DefinitiveFailure
        },
        DocumentControlReceiveOutcome::CommandOutcome(
            DocumentControlOutcome::DriveOneTurnOutcomeIndeterminate { .. } |
            DocumentControlOutcome::AdvanceOutcomeIndeterminate { .. } |
            DocumentControlOutcome::AutomationOutcomeIndeterminate { .. },
        ) |
        DocumentControlReceiveOutcome::DriveOneTurnOutcomeIndeterminate(_) => {
            ControlOutcomeDisposition::Indeterminate
        },
    }
}

#[cfg(test)]
mod tests {
    use servo::document_control::{DocumentControlError, DocumentControlTransportFailure};

    use super::*;

    #[test]
    fn a_definitive_engine_rejection_stays_distinct_from_indeterminate_work() {
        let completion =
            ControlOperationCompletion::new(DocumentControlReceiveOutcome::CommandOutcome(
                DocumentControlOutcome::Rejected(DocumentControlError::NotControlled),
            ));

        assert_eq!(
            completion.disposition(),
            ControlOutcomeDisposition::DefinitiveFailure
        );
    }

    #[test]
    fn a_failed_observation_does_not_claim_page_mutation() {
        let completion = ControlOperationCompletion::new(
            DocumentControlReceiveOutcome::ObserveTransportFailure(
                DocumentControlTransportFailure::TimedOut,
            ),
        );

        assert_eq!(
            completion.disposition(),
            ControlOutcomeDisposition::DefinitiveFailure
        );
    }

    #[test]
    fn a_missing_read_only_automation_response_is_definitive() {
        let completion = ControlOperationCompletion::new(
            DocumentControlReceiveOutcome::AutomationTransportFailure(
                DocumentControlTransportFailure::Disconnected,
            ),
        );

        assert_eq!(
            completion.disposition(),
            ControlOutcomeDisposition::DefinitiveFailure
        );
    }

    #[test]
    fn a_missing_drive_response_is_indeterminate_and_not_retryable() {
        let completion = ControlOperationCompletion::new(
            DocumentControlReceiveOutcome::DriveOneTurnOutcomeIndeterminate(
                DocumentControlTransportFailure::Disconnected,
            ),
        );

        assert_eq!(
            completion.disposition(),
            ControlOutcomeDisposition::Indeterminate
        );
    }
}
