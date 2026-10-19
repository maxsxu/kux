//! Reloadable polling cadence; watch publication keeps its independent coalescing rate.
use std::time::Duration;
use tokio::{
    sync::watch,
    time::{Interval, MissedTickBehavior},
};

pub(super) struct RefreshSchedule {
    updates: watch::Receiver<Duration>,
    ticker: Interval,
    open: bool,
}

impl RefreshSchedule {
    pub(super) fn new(mut updates: watch::Receiver<Duration>) -> Self {
        let ticker = Self::interval(*updates.borrow_and_update());
        Self {
            updates,
            ticker,
            open: true,
        }
    }

    fn interval(duration: Duration) -> Interval {
        let mut ticker = tokio::time::interval(duration.max(Duration::from_secs(2)));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        ticker
    }

    pub(super) async fn tick(&mut self) {
        loop {
            tokio::select! {
                _ = self.ticker.tick() => return,
                changed = self.updates.changed(), if self.open => {
                    if changed.is_err() { self.open = false; }
                    else { self.ticker = Self::interval(*self.updates.borrow_and_update()); }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cadence_reloads_without_restarting_watch_and_survives_sender_drop() {
        let (sender, receiver) = watch::channel(Duration::from_secs(600));
        let mut cadence = RefreshSchedule::new(receiver);
        cadence.tick().await;
        assert!(
            tokio::time::timeout(Duration::from_millis(20), cadence.tick())
                .await
                .is_err()
        );
        sender.send(Duration::from_secs(2)).unwrap();
        tokio::time::timeout(Duration::from_millis(100), cadence.tick())
            .await
            .unwrap();
        drop(sender);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), cadence.tick())
                .await
                .is_err()
        );
    }
}
