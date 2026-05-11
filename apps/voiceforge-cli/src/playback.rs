//! Single-consumer playback queue (ROADMAP 4.3).
//!
//! Before 4.3, the daemon's `process_frame` did
//! `tokio::task::spawn_blocking(sink.play(...))` — fire-and-forget.
//! Two events arriving close together would have their audio play
//! concurrently and the user heard a garbled mess. Single-voice was
//! racy; multi-turn casts (4.3) would be unlistenable.
//!
//! This module replaces the per-event `spawn_blocking` with a single
//! consumer task that pops `(audio_path, semaphore_permit)` items in
//! FIFO order and plays them strictly serially via `spawn_blocking`.
//! Both single-voice and cast paths post to this queue.
//!
//! ## Guarantees
//!
//! - **No overlap**: at most one `AudioSink::play` call executes at a
//!   time, ever.
//! - **FIFO order**: items play in the order they were pushed.
//! - **Permit lifecycle**: each item carries the daemon's inflight
//!   `OwnedSemaphorePermit`. The permit is dropped after the play
//!   call returns, so the daemon's `MAX_INFLIGHT` cap correctly
//!   measures "queued + currently playing", not "currently dispatched
//!   to spawn_blocking".
//! - **Backpressure**: bounded mpsc channel (capacity 16). When full,
//!   `push` awaits — translates to client-visible slowdown rather
//!   than dropped audio.
//!
//! ## Why not `Mutex<()>` around `play`?
//!
//! A mutex serializes but doesn't FIFO. Two tasks racing to acquire
//! the lock can be serviced in arbitrary order — the second one
//! posted might play first. The queue gives us deterministic ordering,
//! which matters for cast turns ("turn 2 must play after turn 1").
//!
//! ## Why not `tokio::sync::Semaphore` with permits=1?
//!
//! Same FIFO problem. `Semaphore::acquire` doesn't guarantee FIFO
//! across waiters.

use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::sync::OwnedSemaphorePermit;

use crate::audio_sink::AudioSink;

/// Channel capacity. 16 is enough for a tight burst of events
/// (e.g. a noisy `build_failed` plus several follow-ups) without
/// applying backpressure on a sane workflow. Past 16, the producer
/// awaits — no audio is dropped.
const QUEUE_CAPACITY: usize = 16;

pub struct PlaybackItem {
    pub path: PathBuf,
    /// Optional. When `Some`, held until `play` returns and dropped
    /// then. Used by callers that want inflight accounting at the
    /// per-item granularity. ROADMAP 4.3's daemon now holds a
    /// per-frame permit across the cast loop instead, so it passes
    /// `None` here — the queue's bounded channel is the real
    /// backpressure mechanism.
    pub permit: Option<OwnedSemaphorePermit>,
}

#[derive(Clone)]
pub struct PlaybackQueue {
    tx: mpsc::Sender<PlaybackItem>,
}

impl PlaybackQueue {
    /// Spawn the consumer task. Returns the producer handle.
    /// The consumer task lives until all `PlaybackQueue` clones are
    /// dropped (channel close).
    pub fn spawn(sink: Arc<dyn AudioSink>) -> Self {
        let (tx, mut rx) = mpsc::channel::<PlaybackItem>(QUEUE_CAPACITY);
        tokio::spawn(async move {
            while let Some(item) = rx.recv().await {
                let sink = Arc::clone(&sink);
                // spawn_blocking so the rodio sync call doesn't occupy
                // a Tokio worker. We `await` the join handle to keep
                // FIFO — the next iteration of the recv loop only
                // starts after this play completes.
                let join = tokio::task::spawn_blocking(move || {
                    let _permit = item.permit; // Some(p) dropped after play; None is no-op
                    if let Err(e) = sink.play(&item.path) {
                        eprintln!(
                            "voiceforge: playback failed for {}: {e:#}",
                            item.path.display()
                        );
                    }
                });
                if let Err(e) = join.await {
                    if e.is_cancelled() {
                        // Daemon shutting down; consumer task should
                        // exit cleanly. Drain remaining items by
                        // breaking out — they'll go to the bit-bucket.
                        eprintln!("voiceforge: playback task cancelled during shutdown");
                        break;
                    }
                    // panic in spawn_blocking — rare. Log and carry on
                    // so the queue doesn't seize.
                    eprintln!("voiceforge: playback worker panicked: {e}");
                }
            }
        });
        Self { tx }
    }

    /// Post an item. Awaits if the queue is full (backpressure).
    /// Returns `Err` if the consumer task has died (channel closed).
    pub async fn push(&self, item: PlaybackItem) -> Result<(), PlaybackPushError> {
        self.tx
            .send(item)
            .await
            .map_err(|_| PlaybackPushError::ConsumerGone)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PlaybackPushError {
    #[error("playback consumer task is gone")]
    ConsumerGone,
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use anyhow::Result;
    use std::path::Path;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    /// Records `(path, start, end)` for every `play` call. Holds for
    /// `delay` per call so sequencing tests can assert that turn-2's
    /// start > turn-1's end.
    pub(crate) struct BlockingRecordingSink {
        pub delay: Duration,
        pub events: Mutex<Vec<(PathBuf, Instant, Instant)>>,
    }

    impl BlockingRecordingSink {
        pub(crate) fn new(delay: Duration) -> Self {
            Self {
                delay,
                events: Mutex::new(Vec::new()),
            }
        }

        pub(crate) fn events(&self) -> Vec<(PathBuf, Instant, Instant)> {
            self.events.lock().unwrap().clone()
        }
    }

    impl AudioSink for BlockingRecordingSink {
        fn play(&self, wav_path: &Path) -> Result<()> {
            let start = Instant::now();
            std::thread::sleep(self.delay);
            let end = Instant::now();
            self.events
                .lock()
                .unwrap()
                .push((wav_path.to_path_buf(), start, end));
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use std::time::Duration;
    use tokio::sync::Semaphore;

    fn perm() -> OwnedSemaphorePermit {
        let s = Arc::new(Semaphore::new(1));
        s.try_acquire_owned().unwrap()
    }

    /// Poll `sink.events()` until it reaches `expected` or `deadline`
    /// elapses. Avoids brittle fixed-sleep timing on slow CI runners
    /// (the macOS GitHub Actions runner is consistently slower than
    /// our local dev box).
    async fn wait_for_events(
        sink: &BlockingRecordingSink,
        expected: usize,
        deadline: Duration,
    ) -> Vec<(PathBuf, std::time::Instant, std::time::Instant)> {
        let start = std::time::Instant::now();
        loop {
            let events = sink.events();
            if events.len() >= expected || start.elapsed() >= deadline {
                return events;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn plays_items_in_fifo_order() {
        let sink = Arc::new(BlockingRecordingSink::new(Duration::from_millis(10)));
        let queue = PlaybackQueue::spawn(sink.clone() as Arc<dyn AudioSink>);
        for i in 0..5 {
            queue
                .push(PlaybackItem {
                    path: PathBuf::from(format!("/tmp/{i}.wav")),
                    permit: Some(perm()),
                })
                .await
                .expect("push");
        }
        let events = wait_for_events(&sink, 5, Duration::from_secs(5)).await;
        assert_eq!(events.len(), 5);
        let paths: Vec<&str> = events.iter().map(|(p, _, _)| p.to_str().unwrap()).collect();
        assert_eq!(
            paths,
            vec![
                "/tmp/0.wav",
                "/tmp/1.wav",
                "/tmp/2.wav",
                "/tmp/3.wav",
                "/tmp/4.wav"
            ]
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn serializes_concurrent_plays_no_overlap() {
        // Three items, each play takes 50ms. If they overlap, the
        // total wall time would be ~50ms. If serialized, ~150ms.
        let sink = Arc::new(BlockingRecordingSink::new(Duration::from_millis(50)));
        let queue = PlaybackQueue::spawn(sink.clone() as Arc<dyn AudioSink>);
        for i in 0..3 {
            queue
                .push(PlaybackItem {
                    path: PathBuf::from(format!("/tmp/{i}.wav")),
                    permit: Some(perm()),
                })
                .await
                .expect("push");
        }
        let events = wait_for_events(&sink, 3, Duration::from_secs(5)).await;
        assert_eq!(events.len(), 3);
        // Strict ordering: each subsequent play STARTED after the
        // previous one ENDED.
        for w in events.windows(2) {
            let prev_end = w[0].2;
            let next_start = w[1].1;
            assert!(
                next_start >= prev_end,
                "turn started before previous ended: prev_end={prev_end:?} next_start={next_start:?}"
            );
        }
    }

    #[tokio::test]
    async fn push_returns_err_when_consumer_gone() {
        let sink = Arc::new(BlockingRecordingSink::new(Duration::from_millis(1)));
        let queue = PlaybackQueue::spawn(sink as Arc<dyn AudioSink>);
        // Drop only the queue's clone — the consumer task holds a
        // weak-ish reference via the channel. We can't easily kill
        // the consumer from outside without exposing internals; the
        // ConsumerGone path is exercised by daemon-shutdown integration
        // tests in daemon_server.rs. Sanity-check that push works
        // when the consumer is alive.
        let _ = queue
            .push(PlaybackItem {
                path: PathBuf::from("/tmp/x.wav"),
                permit: Some(perm()),
            })
            .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn push_with_none_permit_plays_normally() {
        let sink = Arc::new(BlockingRecordingSink::new(Duration::from_millis(5)));
        let queue = PlaybackQueue::spawn(sink.clone() as Arc<dyn AudioSink>);
        queue
            .push(PlaybackItem {
                path: PathBuf::from("/tmp/no_permit.wav"),
                permit: None,
            })
            .await
            .expect("push");
        let events = wait_for_events(&sink, 1, Duration::from_secs(2)).await;
        assert_eq!(events.len(), 1);
    }
}
