//! The calls the whole install may send GD Studio's API: a sliding window shared by every
//! search, download and health check, taken in the order they ask, one call at a time. A call
//! is counted the moment it is sent, and the window is kept on disk so a restart inside it
//! does not start it empty.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::Instant;
use tracing::warn;

pub(crate) struct Budget {
    calls: usize,
    window: Duration,
    /// When each call of the window went out, oldest first. Held from when a caller waits for
    /// a slot until its call has its answer, so the callers get theirs in the order they asked
    /// and no slot is taken by a call not yet sent.
    sent: tokio::sync::Mutex<VecDeque<Instant>>,
    /// The file the window is kept in, one send time in Unix milliseconds a line; None in tests
    /// that keep it in memory.
    kept: Option<PathBuf>,
}

impl Budget {
    /// A budget kept in memory only.
    #[cfg(test)]
    pub(crate) fn new(calls: usize, window: Duration) -> Budget {
        Budget {
            calls,
            window,
            sent: tokio::sync::Mutex::new(VecDeque::new()),
            kept: None,
        }
    }

    /// A budget kept in `file`, starting with the calls an earlier run sent less than a window
    /// ago.
    pub(crate) fn kept_in(calls: usize, window: Duration, file: PathBuf) -> Budget {
        let sent = match std::fs::read_to_string(&file) {
            Ok(text) => recent(&text, window),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => VecDeque::new(),
            Err(e) => {
                warn!(
                    "Could not read the GD Studio calls kept in {}: {e}",
                    file.display()
                );
                VecDeque::new()
            }
        };
        Budget {
            calls,
            window,
            sent: tokio::sync::Mutex::new(sent),
            kept: Some(file),
        }
    }

    /// Waits until fewer than `calls` went out in the last `window`, then holds the window
    /// until the slot is dropped. A caller that gives up while waiting, or drops its slot
    /// unspent, takes nothing.
    pub(crate) async fn take(&self) -> Slot<'_> {
        let mut sent = self.sent.lock().await;
        loop {
            let now = Instant::now();
            while sent
                .front()
                .is_some_and(|at| now.duration_since(*at) >= self.window)
            {
                sent.pop_front();
            }
            if sent.len() < self.calls {
                return Slot { budget: self, sent };
            }
            let oldest = *sent.front().expect("a full window has calls");
            tokio::time::sleep_until(oldest + self.window).await;
        }
    }
}

/// The send times in `text` less than `window` old, oldest first, on this process's clock. One
/// stamped in the future, or before this clock can say, counts as sent now.
fn recent(text: &str, window: Duration) -> VecDeque<Instant> {
    let (now, wall) = (Instant::now(), SystemTime::now());
    let mut sent: Vec<Instant> = (text.lines())
        .filter_map(|line| line.trim().parse::<u64>().ok())
        .filter_map(|ms| {
            let age =
                (wall.duration_since(UNIX_EPOCH + Duration::from_millis(ms))).unwrap_or_default();
            (age < window).then(|| now.checked_sub(age).unwrap_or(now))
        })
        .collect();
    sent.sort();
    sent.into()
}

/// A free slot of the window, held by one caller until it is dropped.
pub(crate) struct Slot<'a> {
    budget: &'a Budget,
    sent: tokio::sync::MutexGuard<'a, VecDeque<Instant>>,
}

impl Slot<'_> {
    /// Counts the call as sent now, once, right before it goes out, and keeps the window.
    pub(crate) async fn spend(&mut self) {
        self.sent.push_back(Instant::now());
        let Some(file) = self.budget.kept.clone() else {
            return;
        };
        let (now, wall) = (Instant::now(), SystemTime::now());
        let lines: String = (self.sent.iter())
            .filter_map(|at| wall.checked_sub(now.duration_since(*at)))
            .filter_map(|at| at.duration_since(UNIX_EPOCH).ok())
            .map(|since| format!("{}\n", since.as_millis()))
            .collect();
        let written = tokio::task::spawn_blocking(move || {
            let next = file.with_extension("new");
            std::fs::write(&next, lines)?;
            std::fs::rename(&next, &file)
        })
        .await
        .map_err(std::io::Error::other)
        .and_then(|written| written);
        if let Err(e) = written {
            warn!("Could not keep the GD Studio calls of the last 5 minutes: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    async fn spend(budget: &Budget) {
        budget.take().await.spend().await;
    }

    #[tokio::test(start_paused = true)]
    async fn forty_five_calls_pass_in_five_minutes_and_the_next_waits_for_the_first_to_leave() {
        let budget = Budget::new(45, Duration::from_secs(300));
        let start = Instant::now();
        for i in 0..45 {
            spend(&budget).await;
            assert_eq!(start.elapsed(), Duration::from_secs(i), "call {i} waited");
            tokio::time::advance(Duration::from_secs(1)).await;
        }
        spend(&budget).await;
        assert_eq!(start.elapsed(), Duration::from_secs(300));
        // The second call of the window leaves one second later.
        spend(&budget).await;
        assert_eq!(start.elapsed(), Duration::from_secs(301));
    }

    #[tokio::test(start_paused = true)]
    async fn callers_take_their_slots_in_the_order_they_asked() {
        let budget = Arc::new(Budget::new(1, Duration::from_secs(10)));
        spend(&budget).await;
        let order = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut waiting = Vec::new();
        for caller in 0..3 {
            let (budget, order) = (budget.clone(), order.clone());
            waiting.push(tokio::spawn(async move {
                spend(&budget).await;
                order.lock().unwrap().push(caller);
            }));
            tokio::task::yield_now().await;
        }
        for task in waiting {
            task.await.unwrap();
        }
        assert_eq!(*order.lock().unwrap(), [0, 1, 2]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_caller_that_gives_up_takes_nothing() {
        let budget = Budget::new(1, Duration::from_secs(10));
        spend(&budget).await;
        let gave_up = tokio::time::timeout(Duration::from_secs(5), budget.take()).await;
        assert!(gave_up.is_err());
        let start = Instant::now();
        spend(&budget).await;
        assert_eq!(start.elapsed(), Duration::from_secs(5));
        // The first call left the window; the one that gave up left no mark in it.
        assert_eq!(budget.sent.lock().await.len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_slot_counts_when_it_is_spent_and_holds_the_window_until_it_is_dropped() {
        let budget = Arc::new(Budget::new(3, Duration::from_secs(10)));
        let mut held = budget.take().await;
        let next = tokio::spawn({
            let budget = budget.clone();
            async move {
                let asked = Instant::now();
                spend(&budget).await;
                asked.elapsed()
            }
        });
        tokio::time::sleep(Duration::from_secs(3)).await;
        held.spend().await;
        tokio::time::sleep(Duration::from_secs(1)).await;
        drop(held);
        // The other caller waited for the held slot, not for room in the window.
        assert_eq!(next.await.unwrap(), Duration::from_secs(4));
        assert_eq!(budget.sent.lock().await.len(), 2);
        // A slot dropped unspent leaves no mark.
        drop(budget.take().await);
        assert_eq!(budget.sent.lock().await.len(), 2);
    }

    #[tokio::test]
    async fn the_calls_of_the_last_window_hold_the_next_run_too() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(".gdstudio-calls");
        let window = Duration::from_secs(300);
        let wall = |ago: u64| {
            (SystemTime::now() - Duration::from_secs(ago))
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis()
        };
        // Two calls of this window, one older, a line that is no time, one from the future.
        std::fs::write(
            &file,
            format!(
                "{}\n{}\nnot a time\n{}\n{}\n",
                wall(400),
                wall(120),
                wall(30),
                wall(0) + 60_000
            ),
        )
        .unwrap();
        let budget = Budget::kept_in(4, window, file.clone());
        assert_eq!(budget.sent.lock().await.len(), 3);
        spend(&budget).await;
        // The window is full: the next call waits about three minutes, for the one sent 120 s
        // before the restart.
        assert!(
            tokio::time::timeout(Duration::from_millis(100), budget.take())
                .await
                .is_err()
        );
        let kept = Budget::kept_in(4, window, file.clone());
        assert_eq!(kept.sent.lock().await.len(), 4);
        assert!(!file.with_extension("new").exists());

        let none = Budget::kept_in(4, window, dir.path().join("never-written"));
        assert!(none.sent.lock().await.is_empty());
    }
}
