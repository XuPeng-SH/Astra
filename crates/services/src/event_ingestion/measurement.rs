//! Opt-in, bounded measurements for individual ingestion deliveries.
//!
//! A delivery reserves report-channel capacity before observation starts. The
//! reservation remains held until the delivery reaches a terminal state, so
//! active observations and unread reports share one strict bound.

use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use astra_core::sync_poison::recover_mutex_lock;
use tokio::sync::mpsc;

/// Opaque caller-provided key correlating a delivery with its terminal report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct IngestionDeliveryKey(pub u64);

/// Why an ingestion delivery was rejected before durable commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngestionRejectionReason {
    TelemetryHeadroom,
    Serialization,
    ResidentLimit,
    DeferredLimit,
    ChannelClosed,
    NoRuntime,
    IdentityCollision,
    SessionAdmission,
}

/// Why the durable outcome of an ingestion delivery is unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngestionUnknownReason {
    AdmissionAfterUncertainCommit,
    DeliveryDropped,
    ShutdownUnresolved,
}

/// The terminal outcome observed for one ingestion delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngestionDeliveryTerminal {
    CommittedInserted,
    CommittedReplayed,
    Rejected(IngestionRejectionReason),
    Unknown(IngestionUnknownReason),
}

/// A process-local phase in the ingestion delivery path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngestionMeasurementPhase {
    Submitted,
    ResidentAdmitted,
    Deferred,
    ChannelAccepted,
    WorkerReceived,
    Dispatched,
}

/// Timing and retry progress captured for one ingestion delivery.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IngestionDeliveryProgress {
    pub submitted_at: Option<Instant>,
    pub resident_admitted_at: Option<Instant>,
    pub deferred_at: Option<Instant>,
    pub channel_accepted_at: Option<Instant>,
    pub worker_received_at: Option<Instant>,
    pub first_dispatched_at: Option<Instant>,
    pub attempt_count: u64,
    pub limiter_wait: Duration,
    pub pool_wait: Duration,
    pub transaction_time: Duration,
    pub commit_was_uncertain: bool,
}

/// The terminal report for one observed ingestion delivery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestionDeliveryReport {
    pub key: IngestionDeliveryKey,
    pub terminal: IngestionDeliveryTerminal,
    pub terminal_at: Instant,
    pub progress: IngestionDeliveryProgress,
}

/// A non-blocking producer for opt-in ingestion delivery measurements.
#[derive(Clone, Debug)]
pub struct IngestionMeasurementSink {
    tx: mpsc::Sender<IngestionDeliveryReport>,
}

impl IngestionMeasurementSink {
    /// Create a sink and receiver sharing a bounded terminal-report channel.
    ///
    /// As with [`mpsc::channel`], `capacity` must be greater than zero.
    pub fn bounded(capacity: usize) -> (Self, IngestionMeasurementReceiver) {
        let (tx, rx) = mpsc::channel(capacity);
        (Self { tx }, rx)
    }

    /// Start observing one delivery without waiting for report capacity.
    pub fn try_start(
        &self,
        key: IngestionDeliveryKey,
    ) -> Result<(IngestionDeliveryToken, IngestionDeliveryProbe), MeasurementUnavailable> {
        let permit = match self.tx.clone().try_reserve_owned() {
            Ok(permit) => permit,
            Err(mpsc::error::TrySendError::Full(_)) => {
                return Err(MeasurementUnavailable::Full);
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                return Err(MeasurementUnavailable::Closed);
            }
        };

        let state = Arc::new(DeliveryState::new());
        let guard = Arc::new(DeliveryGuard {
            key,
            state: Arc::clone(&state),
            permit: Mutex::new(Some(permit)),
        });

        Ok((
            IngestionDeliveryToken { observation: guard },
            IngestionDeliveryProbe { state },
        ))
    }
}

/// Receives terminal reports from an [`IngestionMeasurementSink`].
pub type IngestionMeasurementReceiver = mpsc::Receiver<IngestionDeliveryReport>;

/// Failure to reserve bounded measurement capacity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeasurementUnavailable {
    Full,
    Closed,
}

/// Non-clone handoff proving that one delivery owns a measurement reservation.
pub struct IngestionDeliveryToken {
    observation: Arc<DeliveryGuard>,
}

impl fmt::Debug for IngestionDeliveryToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IngestionDeliveryToken")
            .finish_non_exhaustive()
    }
}

impl IngestionDeliveryToken {
    pub(super) fn into_observation(self) -> Arc<DeliveryGuard> {
        self.observation
    }
}

/// Read-only access to live delivery progress.
pub struct IngestionDeliveryProbe {
    state: Arc<DeliveryState>,
}

impl fmt::Debug for IngestionDeliveryProbe {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IngestionDeliveryProbe")
            .field("progress", &self.snapshot())
            .finish()
    }
}

impl IngestionDeliveryProbe {
    pub fn snapshot(&self) -> IngestionDeliveryProgress {
        self.state.snapshot()
    }
}

impl DeliveryGuard {
    pub(super) fn mark(&self, phase: IngestionMeasurementPhase) {
        self.state.mark(phase);
    }

    pub(super) fn start_attempt(&self) {
        self.state.start_attempt();
    }

    pub(super) fn add_limiter_wait(&self, duration: Duration) {
        self.state.add_limiter_wait(duration);
    }

    pub(super) fn add_pool_wait(&self, duration: Duration) {
        self.state.add_pool_wait(duration);
    }

    pub(super) fn add_transaction_time(&self, duration: Duration) {
        self.state.add_transaction_time(duration);
    }

    pub(super) fn commit_started(&self) {
        self.state.commit_started();
    }

    pub(super) fn commit_acknowledged(&self) {
        self.state.commit_acknowledged();
    }

    pub(super) fn commit_failed(&self) {
        self.state.commit_failed();
    }
}

pub(super) struct DeliveryGuard {
    key: IngestionDeliveryKey,
    state: Arc<DeliveryState>,
    permit: Mutex<Option<mpsc::OwnedPermit<IngestionDeliveryReport>>>,
}

impl DeliveryGuard {
    pub(super) fn finish(&self, terminal: IngestionDeliveryTerminal) {
        let Some((terminal_at, progress)) = self.state.finish() else {
            return;
        };
        // A session fence can prevent a retry from checking a prior lost-ack
        // commit. Preserve uncertainty instead of asserting non-persistence.
        let terminal = if progress.commit_was_uncertain
            && matches!(
                terminal,
                IngestionDeliveryTerminal::Rejected(IngestionRejectionReason::SessionAdmission)
            ) {
            IngestionDeliveryTerminal::Unknown(
                IngestionUnknownReason::AdmissionAfterUncertainCommit,
            )
        } else {
            terminal
        };
        let permit = recover_mutex_lock(&self.permit).take();
        if let Some(permit) = permit {
            let _sender = permit.send(IngestionDeliveryReport {
                key: self.key,
                terminal,
                terminal_at,
                progress,
            });
        }
    }
}

impl Drop for DeliveryGuard {
    fn drop(&mut self) {
        self.finish(IngestionDeliveryTerminal::Unknown(
            IngestionUnknownReason::DeliveryDropped,
        ));
    }
}

struct DeliveryState {
    inner: Mutex<DeliveryStateInner>,
}

struct DeliveryStateInner {
    progress: IngestionDeliveryProgress,
    commit_in_progress: bool,
    finished: bool,
}

impl DeliveryState {
    fn new() -> Self {
        Self {
            inner: Mutex::new(DeliveryStateInner {
                progress: IngestionDeliveryProgress::default(),
                commit_in_progress: false,
                finished: false,
            }),
        }
    }

    fn snapshot(&self) -> IngestionDeliveryProgress {
        recover_mutex_lock(&self.inner).progress.clone()
    }

    fn update(&self, update: impl FnOnce(&mut DeliveryStateInner)) {
        let mut inner = recover_mutex_lock(&self.inner);
        if !inner.finished {
            update(&mut inner);
        }
    }

    fn mark(&self, phase: IngestionMeasurementPhase) {
        let now = Instant::now();
        let mut inner = recover_mutex_lock(&self.inner);
        if inner.finished {
            return;
        }
        let timestamp = match phase {
            IngestionMeasurementPhase::Submitted => &mut inner.progress.submitted_at,
            IngestionMeasurementPhase::ResidentAdmitted => &mut inner.progress.resident_admitted_at,
            IngestionMeasurementPhase::Deferred => &mut inner.progress.deferred_at,
            IngestionMeasurementPhase::ChannelAccepted => &mut inner.progress.channel_accepted_at,
            IngestionMeasurementPhase::WorkerReceived => &mut inner.progress.worker_received_at,
            IngestionMeasurementPhase::Dispatched => &mut inner.progress.first_dispatched_at,
        };
        timestamp.get_or_insert(now);
    }

    fn start_attempt(&self) {
        self.update(|inner| {
            inner.progress.attempt_count = inner.progress.attempt_count.saturating_add(1);
        });
    }

    fn add_limiter_wait(&self, duration: Duration) {
        self.update(|inner| {
            inner.progress.limiter_wait = inner.progress.limiter_wait.saturating_add(duration);
        });
    }

    fn add_pool_wait(&self, duration: Duration) {
        self.update(|inner| {
            inner.progress.pool_wait = inner.progress.pool_wait.saturating_add(duration);
        });
    }

    fn add_transaction_time(&self, duration: Duration) {
        self.update(|inner| {
            inner.progress.transaction_time =
                inner.progress.transaction_time.saturating_add(duration);
        });
    }

    fn commit_started(&self) {
        self.update(|inner| inner.commit_in_progress = true);
    }

    fn commit_acknowledged(&self) {
        self.update(|inner| inner.commit_in_progress = false);
    }

    fn commit_failed(&self) {
        self.update(|inner| {
            if inner.commit_in_progress {
                inner.commit_in_progress = false;
                inner.progress.commit_was_uncertain = true;
            }
        });
    }

    fn finish(&self) -> Option<(Instant, IngestionDeliveryProgress)> {
        let terminal_at = Instant::now();
        let mut inner = recover_mutex_lock(&self.inner);
        if inner.finished {
            return None;
        }
        inner.finished = true;
        if inner.commit_in_progress {
            inner.commit_in_progress = false;
            inner.progress.commit_was_uncertain = true;
        }
        Some((terminal_at, inner.progress.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn committed() -> IngestionDeliveryTerminal {
        IngestionDeliveryTerminal::CommittedInserted
    }

    #[tokio::test]
    async fn session_admission_cannot_resolve_a_prior_uncertain_commit() {
        let (sink, mut receiver) = IngestionMeasurementSink::bounded(2);
        for (key, terminal, expected) in [
            (
                1,
                IngestionDeliveryTerminal::Rejected(IngestionRejectionReason::SessionAdmission),
                IngestionDeliveryTerminal::Unknown(
                    IngestionUnknownReason::AdmissionAfterUncertainCommit,
                ),
            ),
            (
                2,
                IngestionDeliveryTerminal::CommittedReplayed,
                IngestionDeliveryTerminal::CommittedReplayed,
            ),
        ] {
            let (token, _) = sink.try_start(IngestionDeliveryKey(key)).unwrap();
            let observation = token.into_observation();
            observation.commit_started();
            observation.commit_failed();
            observation.start_attempt();
            observation.finish(terminal);
            let report = receiver.recv().await.unwrap();
            assert!(report.progress.commit_was_uncertain);
            assert_eq!(report.terminal, expected);
        }
    }

    #[tokio::test]
    async fn reserved_capacity_covers_active_and_queued_reports() {
        let (sink, mut receiver) = IngestionMeasurementSink::bounded(2);
        let (first, _) = sink.try_start(IngestionDeliveryKey(1)).unwrap();
        let (second, _) = sink.try_start(IngestionDeliveryKey(2)).unwrap();
        assert_eq!(
            sink.try_start(IngestionDeliveryKey(3)).unwrap_err(),
            MeasurementUnavailable::Full
        );

        let first = first.into_observation();
        first.finish(committed());
        assert_eq!(
            sink.try_start(IngestionDeliveryKey(3)).unwrap_err(),
            MeasurementUnavailable::Full,
            "an unread report must retain its channel slot"
        );

        let report = receiver.recv().await.unwrap();
        assert_eq!(report.key, IngestionDeliveryKey(1));
        assert!(sink.try_start(IngestionDeliveryKey(3)).is_ok());

        drop(second);
    }

    #[tokio::test]
    async fn final_observation_clone_emits_delivery_dropped_once() {
        let (sink, mut receiver) = IngestionMeasurementSink::bounded(1);
        let (token, _) = sink.try_start(IngestionDeliveryKey(7)).unwrap();
        let observation = token.into_observation();
        let clone = observation.clone();

        drop(observation);
        assert_eq!(receiver.try_recv(), Err(mpsc::error::TryRecvError::Empty));
        drop(clone);

        let report = receiver.recv().await.unwrap();
        assert_eq!(report.key, IngestionDeliveryKey(7));
        assert_eq!(
            report.terminal,
            IngestionDeliveryTerminal::Unknown(IngestionUnknownReason::DeliveryDropped)
        );
        assert_eq!(receiver.try_recv(), Err(mpsc::error::TryRecvError::Empty));
    }

    #[tokio::test]
    async fn probe_does_not_retain_the_reserved_permit() {
        let (sink, mut receiver) = IngestionMeasurementSink::bounded(1);
        let (token, probe) = sink.try_start(IngestionDeliveryKey(10)).unwrap();

        drop(token);
        let report = receiver.recv().await.unwrap();
        assert_eq!(report.key, IngestionDeliveryKey(10));
        assert_eq!(probe.snapshot(), report.progress);

        assert!(sink.try_start(IngestionDeliveryKey(11)).is_ok());
        drop(probe);
    }

    #[tokio::test]
    async fn explicit_finish_beats_final_drop() {
        let (sink, mut receiver) = IngestionMeasurementSink::bounded(1);
        let (token, _) = sink.try_start(IngestionDeliveryKey(20)).unwrap();
        let observation = token.into_observation();
        let clone = observation.clone();

        observation.finish(IngestionDeliveryTerminal::CommittedReplayed);
        drop(observation);
        drop(clone);

        let report = receiver.recv().await.unwrap();
        assert_eq!(
            report.terminal,
            IngestionDeliveryTerminal::CommittedReplayed
        );
        assert_eq!(receiver.try_recv(), Err(mpsc::error::TryRecvError::Empty));
    }

    #[test]
    fn receiver_closure_is_harmless_to_active_delivery() {
        let (sink, receiver) = IngestionMeasurementSink::bounded(1);
        let (token, _) = sink.try_start(IngestionDeliveryKey(30)).unwrap();
        let observation = token.into_observation();

        drop(receiver);
        observation.mark(IngestionMeasurementPhase::Submitted);
        observation.finish(committed());
        drop(observation);

        assert_eq!(
            sink.try_start(IngestionDeliveryKey(31)).unwrap_err(),
            MeasurementUnavailable::Closed
        );
    }

    #[tokio::test]
    async fn commit_retry_retains_uncertainty() {
        let (sink, mut receiver) = IngestionMeasurementSink::bounded(1);
        let (token, probe) = sink.try_start(IngestionDeliveryKey(40)).unwrap();
        let observation = token.into_observation();

        observation.commit_failed();
        assert!(!probe.snapshot().commit_was_uncertain);

        observation.start_attempt();
        observation.commit_started();
        observation.commit_failed();
        observation.start_attempt();
        observation.commit_started();
        observation.commit_acknowledged();
        observation.finish(committed());

        let report = receiver.recv().await.unwrap();
        assert_eq!(report.progress.attempt_count, 2);
        assert!(report.progress.commit_was_uncertain);
    }

    #[tokio::test]
    async fn drop_during_commit_marks_the_report_uncertain() {
        let (sink, mut receiver) = IngestionMeasurementSink::bounded(1);
        let (token, _) = sink.try_start(IngestionDeliveryKey(50)).unwrap();
        let observation = token.into_observation();
        observation.commit_started();

        drop(observation);

        let report = receiver.recv().await.unwrap();
        assert_eq!(
            report.terminal,
            IngestionDeliveryTerminal::Unknown(IngestionUnknownReason::DeliveryDropped)
        );
        assert!(report.progress.commit_was_uncertain);
    }
}
