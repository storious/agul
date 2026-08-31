use std::future::poll_fn;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Poll, Waker};

/// One turn-scoped signal shared by the terminal, provider, and tool runners.
///
/// Cancellation is deliberately separate from presentation callbacks: a turn
/// can be waiting for its first model byte or for a silent child process and
/// still needs to stop promptly.
#[derive(Clone, Debug, Default)]
pub(crate) struct TurnCancellation {
    inner: Arc<TurnCancellationInner>,
}

#[derive(Debug, Default)]
struct TurnCancellationInner {
    cancelled: AtomicBool,
    waker: Mutex<Option<Waker>>,
}

impl TurnCancellation {
    pub(crate) fn cancel(&self) {
        self.inner.cancelled.store(true, Ordering::Release);
        let waker = self
            .inner
            .waker
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::Acquire)
    }

    pub(crate) async fn cancelled(&self) {
        poll_fn(|context| {
            if self.is_cancelled() {
                return Poll::Ready(());
            }
            let mut waker = self
                .inner
                .waker
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if self.is_cancelled() {
                return Poll::Ready(());
            }
            if !waker
                .as_ref()
                .is_some_and(|current| current.will_wake(context.waker()))
            {
                *waker = Some(context.waker().clone());
            }
            Poll::Pending
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::sync::atomic::AtomicBool;
    use std::task::{Context, Wake, Waker};

    use super::*;

    #[test]
    fn clones_observe_the_same_turn_signal() {
        let cancellation = TurnCancellation::default();
        let worker = cancellation.clone();

        assert!(!worker.is_cancelled());
        cancellation.cancel();
        assert!(worker.is_cancelled());
    }

    #[test]
    fn cancellation_wakes_an_async_waiter_without_polling() {
        #[derive(Default)]
        struct WakeFlag(AtomicBool);

        impl Wake for WakeFlag {
            fn wake(self: Arc<Self>) {
                self.0.store(true, Ordering::Release);
            }
        }

        let cancellation = TurnCancellation::default();
        let wake_flag = Arc::new(WakeFlag::default());
        let waker = Waker::from(Arc::clone(&wake_flag));
        let mut context = Context::from_waker(&waker);
        let mut waiting = Box::pin(cancellation.cancelled());

        assert_eq!(waiting.as_mut().poll(&mut context), Poll::Pending);
        cancellation.cancel();
        assert!(wake_flag.0.load(Ordering::Acquire));
        assert_eq!(waiting.as_mut().poll(&mut context), Poll::Ready(()));
    }
}
