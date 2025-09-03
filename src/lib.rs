#![doc = include_str!("../README.md")]

use std::{
    pin::Pin,
    sync::{atomic::AtomicBool, Arc, Mutex},
    task::{Poll, Waker},
};

use futures_util::{Future, Stream};
use pin_project_lite::pin_project;
use smallvec::SmallVec;

/// An intermediary that transfers values from stream to its consumer
pub struct StreamEmitter<T> {
    inner: Arc<Mutex<Inner<T>>>,
}

/// An intermediary that transfers values from stream to its consumer
pub struct TryStreamEmitter<T, E> {
    inner: Arc<Mutex<Inner<Result<T, E>>>>,
}

struct Inner<T> {
    // polling is `true` only for duration of a single `FnStream::poll()` call, which helps detecting invalid usage (cross-thread or cross-task)
    polling: AtomicBool,
    // Due to internal concurrency, a single call to stream's future may yield multiple elements.
    // All elements are stored here and yielded from the stream before polling the future again.
    pending_values: SmallVec<[T; 1]>,
    pending_wakers: SmallVec<[Waker; 1]>,
}

pin_project! {
    /// Implementation of [`Stream`] trait created by [`fn_stream`].
    pub struct FnStream<T, Fut: Future<Output = ()>> {
        #[pin]
        fut: Fut,
        inner: Arc<Mutex<Inner<T>>>,
    }
}

/// Create a new infallible stream which is implemented by `func`.
///
/// Caller should pass an async function which will return successive stream elements via [`StreamEmitter::emit`].
///
/// # Example
///
/// ```rust
/// use async_fn_stream::fn_stream;
/// use futures_util::Stream;
///
/// fn build_stream() -> impl Stream<Item = i32> {
///     fn_stream(|emitter| async move {
///         for i in 0..3 {
///             // yield elements from stream via `emitter`
///             emitter.emit(i).await;
///         }
///     })
/// }
/// ```
pub fn fn_stream<T, Fut: Future<Output = ()>>(
    func: impl FnOnce(StreamEmitter<T>) -> Fut,
) -> FnStream<T, Fut> {
    FnStream::new(func)
}

impl<T, Fut: Future<Output = ()>> FnStream<T, Fut> {
    fn new<F: FnOnce(StreamEmitter<T>) -> Fut>(func: F) -> Self {
        let inner = Arc::new(Mutex::new(Inner {
            polling: AtomicBool::new(false),
            pending_values: SmallVec::new(),
            pending_wakers: SmallVec::new(),
        }));
        let emitter = StreamEmitter {
            inner: inner.clone(),
        };
        let fut = func(emitter);
        Self { fut, inner }
    }
}

impl<T, Fut: Future<Output = ()>> Stream for FnStream<T, Fut> {
    type Item = T;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        let this = self.project();

        let mut inner_guard = this.inner.lock().expect("mutex was poisoned");
        if let Some(value) = inner_guard.pending_values.pop() {
            return Poll::Ready(Some(value));
        }
        if !inner_guard.pending_wakers.is_empty() {
            for waker in inner_guard.pending_wakers.drain(..) {
                if !waker.will_wake(cx.waker()) {
                    waker.wake();
                }
            }
        }

        let old_polling = inner_guard
            .polling
            .swap(true, std::sync::atomic::Ordering::Relaxed);
        drop(inner_guard);
        assert!(
            !old_polling,
            "async-fn-stream invariant violation: polling must be false before entering poll"
        );
        let r = this.fut.poll(cx);
        let mut inner_guard = this.inner.lock().expect("mutex was poisoned");
        inner_guard
            .polling
            .store(false, std::sync::atomic::Ordering::Relaxed);
        match r {
            std::task::Poll::Ready(()) => Poll::Ready(None),
            std::task::Poll::Pending => {
                if let Some(value) = inner_guard.pending_values.pop() {
                    Poll::Ready(Some(value))
                } else {
                    Poll::Pending
                }
            }
        }
    }
}

/// Create a new fallible stream which is implemented by `func`.
///
/// Caller should pass an async function which can:
///
/// - return successive stream elements via [`StreamEmitter::emit`]
/// - return transient errors via [`StreamEmitter::emit_err`]
/// - return fatal errors as [`Result::Err`]
///
/// # Example
/// ```rust
/// use async_fn_stream::try_fn_stream;
/// use futures_util::Stream;
///
/// fn build_stream() -> impl Stream<Item = Result<i32, anyhow::Error>> {
///     try_fn_stream(|emitter| async move {
///         for i in 0..3 {
///             // yield elements from stream via `emitter`
///             emitter.emit(i).await;
///         }
///
///         // return errors view emitter without ending the stream
///         emitter.emit_err(anyhow::anyhow!("An error happened"));
///
///         // return errors from stream, ending the stream
///         Err(anyhow::anyhow!("An error happened"))
///     })
/// }
/// ```
pub fn try_fn_stream<T, E, Fut: Future<Output = Result<(), E>>>(
    func: impl FnOnce(TryStreamEmitter<T, E>) -> Fut,
) -> TryFnStream<T, E, Fut> {
    TryFnStream::new(func)
}

pin_project! {
    /// Implementation of [`Stream`] trait created by [`try_fn_stream`].
    pub struct TryFnStream<T, E, Fut: Future<Output = Result<(), E>>> {
        is_err: bool,
        #[pin]
        fut: Fut,
        inner: Arc<Mutex<Inner<Result<T, E>>>>,
    }
}

impl<T, E, Fut: Future<Output = Result<(), E>>> TryFnStream<T, E, Fut> {
    fn new<F: FnOnce(TryStreamEmitter<T, E>) -> Fut>(func: F) -> Self {
        let inner = Arc::new(Mutex::new(Inner {
            polling: AtomicBool::new(false),
            pending_values: SmallVec::new(),
            pending_wakers: SmallVec::new(),
        }));
        let emitter = TryStreamEmitter {
            inner: inner.clone(),
        };
        let fut = func(emitter);
        Self {
            is_err: false,
            fut,
            inner,
        }
    }
}

impl<T, E, Fut: Future<Output = Result<(), E>>> Stream for TryFnStream<T, E, Fut> {
    type Item = Result<T, E>;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        // TODO: merge the implementation with `FnStream`
        if self.is_err {
            return Poll::Ready(None);
        }
        let this = self.project();
        let mut inner_guard = this.inner.lock().expect("mutex was poisoned");
        if let Some(value) = inner_guard.pending_values.pop() {
            return Poll::Ready(Some(value));
        }
        if !inner_guard.pending_wakers.is_empty() {
            for waker in inner_guard.pending_wakers.drain(..) {
                if !waker.will_wake(cx.waker()) {
                    waker.wake();
                }
            }
        }

        let old_polling = inner_guard
            .polling
            .swap(true, std::sync::atomic::Ordering::Relaxed);
        drop(inner_guard);
        assert!(
            !old_polling,
            "async-fn-stream invariant violation: polling must be false before entering poll"
        );
        let r = this.fut.poll(cx);
        let mut inner_guard = this.inner.lock().expect("mutex was poisoned");
        inner_guard
            .polling
            .store(false, std::sync::atomic::Ordering::Relaxed);
        match r {
            std::task::Poll::Ready(Ok(())) => Poll::Ready(None),
            std::task::Poll::Ready(Err(e)) => {
                *this.is_err = true;
                Poll::Ready(Some(Err(e)))
            }
            std::task::Poll::Pending => {
                if let Some(value) = inner_guard.pending_values.pop() {
                    Poll::Ready(Some(value))
                } else {
                    Poll::Pending
                }
            }
        }
    }
}

impl<T> StreamEmitter<T> {
    /// Emit value from a stream and wait until stream consumer calls [`futures_util::StreamExt::next`] again.
    ///
    /// # Panics
    /// Will panic if:
    /// * `emit` is called twice without awaiting result of first call
    /// * `emit` is called not in context of polling the stream
    #[must_use = "Ensure that emit() is awaited"]
    pub fn emit(&'_ self, value: T) -> CollectFuture<'_, T> {
        CollectFuture::new(&self.inner, value)
    }
}

impl<T, E> TryStreamEmitter<T, E> {
    /// Emit value from a stream and wait until stream consumer calls [`futures_util::StreamExt::next`] again.
    ///
    /// # Panics
    /// Will panic if:
    /// * `emit`/`emit_err` is called twice without awaiting result of the first call
    /// * `emit` is called not in context of polling the stream
    #[must_use = "Ensure that emit() is awaited"]
    pub fn emit(&'_ self, value: T) -> CollectFuture<'_, Result<T, E>> {
        CollectFuture::new(&self.inner, Ok(value))
    }

    /// Emit error from a stream and wait until stream consumer calls [`futures_util::StreamExt::next`] again.
    ///
    /// # Panics
    /// Will panic if:
    /// * `emit`/`emit_err` is called twice without awaiting result of the first call
    /// * `emit_err` is called not in context of polling the stream
    #[must_use = "Ensure that emit_err() is awaited"]
    pub fn emit_err(&'_ self, err: E) -> CollectFuture<'_, Result<T, E>> {
        CollectFuture::new(&self.inner, Err(err))
    }
}

pin_project! {
    /// Future returned from [`StreamEmitter::emit`].
    pub struct CollectFuture<'a, T> {
        inner: &'a Mutex<Inner<T>>,
        value: Option<T>,
    }
}

impl<'a, T> CollectFuture<'a, T> {
    fn new(inner: &'a Mutex<Inner<T>>, value: T) -> Self {
        Self {
            inner,
            value: Some(value),
        }
    }
}

impl<T> Future for CollectFuture<'_, T> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let mut inner_guard = this.inner.lock().expect("Mutex was poisoned");
        let inner = &mut *inner_guard;
        assert!(
            inner.polling.load(std::sync::atomic::Ordering::Relaxed),
            "StreamEmitter::emit().await should only be called in context of `fn_stream()`/`try_fn_stream()`"
        );

        if let Some(value) = this.value.take() {
            inner.pending_values.push(value);
            inner.pending_wakers.push(cx.waker().clone());
            Poll::Pending
        } else if inner.pending_values.is_empty() {
            // stream only polls the future after draining `inner.pending_values`, so this check should not be necessary in theory;
            // this is just a safeguard against misuses; e.g. if a future calls `.emit().poll()` in a loop without yielding on `Poll::Pending`,
            // this would lead to overflow of `inner.pending_values`
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::ErrorKind;

    use futures_util::{pin_mut, stream::FuturesUnordered, StreamExt};

    use super::*;

    #[test]
    fn infallible_works() {
        futures_executor::block_on(async {
            let stream = fn_stream(|collector| async move {
                eprintln!("stream 1");
                collector.emit(1).await;
                eprintln!("stream 2");
                collector.emit(2).await;
                eprintln!("stream 3");
            });
            pin_mut!(stream);
            assert_eq!(Some(1), stream.next().await);
            assert_eq!(Some(2), stream.next().await);
            assert_eq!(None, stream.next().await);
        });
    }

    #[test]
    fn infallible_lifetime() {
        let a = 1;
        futures_executor::block_on(async {
            let b = 2;
            let a = &a;
            let b = &b;
            let stream = fn_stream(|collector| async move {
                eprintln!("stream 1");
                collector.emit(a).await;
                eprintln!("stream 2");
                collector.emit(b).await;
                eprintln!("stream 3");
            });
            pin_mut!(stream);
            assert_eq!(Some(a), stream.next().await);
            assert_eq!(Some(b), stream.next().await);
            assert_eq!(None, stream.next().await);
        });
    }

    #[test]
    fn infallible_unawaited_collects_are_ignored() {
        futures_executor::block_on(async {
            #[expect(
                unused_must_use,
                reason = "this code intentionally does not await collector.emit()"
            )]
            let stream = fn_stream(|collector| async move {
                collector.emit(1)/* .await */;
                collector.emit(2)/* .await */;
                collector.emit(3).await;
            });
            pin_mut!(stream);
            assert_eq!(Some(3), stream.next().await);
            assert_eq!(None, stream.next().await);
        });
    }

    #[test]
    fn fallible_works() {
        futures_executor::block_on(async {
            let stream = try_fn_stream(|collector| async move {
                eprintln!("try stream 1");
                collector.emit(1).await;
                eprintln!("try stream 2");
                collector.emit(2).await;
                eprintln!("try stream 3");
                Err(std::io::Error::from(ErrorKind::Other))
            });
            pin_mut!(stream);
            assert_eq!(1, stream.next().await.unwrap().unwrap());
            assert_eq!(2, stream.next().await.unwrap().unwrap());
            assert!(stream.next().await.unwrap().is_err());
            assert!(stream.next().await.is_none());
        });
    }

    #[test]
    fn fallible_emit_err_works() {
        futures_executor::block_on(async {
            let stream = try_fn_stream(|collector| async move {
                eprintln!("try stream 1");
                collector.emit(1).await;
                eprintln!("try stream 2");
                collector.emit(2).await;
                eprintln!("try stream 3");
                collector
                    .emit_err(std::io::Error::from(ErrorKind::Other))
                    .await;
                eprintln!("try stream 4");
                Err(std::io::Error::from(ErrorKind::Other))
            });
            pin_mut!(stream);
            assert_eq!(1, stream.next().await.unwrap().unwrap());
            assert_eq!(2, stream.next().await.unwrap().unwrap());
            assert!(stream.next().await.unwrap().is_err());
            assert!(stream.next().await.unwrap().is_err());
            assert!(stream.next().await.is_none());
        });
    }

    #[test]
    fn method_async() {
        struct St {
            a: String,
        }

        impl St {
            async fn f1(&self) -> impl Stream<Item = &str> {
                self.f2().await
            }

            #[allow(clippy::unused_async)]
            async fn f2(&self) -> impl Stream<Item = &str> {
                fn_stream(|collector| async move {
                    collector.emit(self.a.as_str()).await;
                    collector.emit(self.a.as_str()).await;
                    collector.emit(self.a.as_str()).await;
                })
            }
        }

        futures_executor::block_on(async {
            let l = St {
                a: "qwe".to_owned(),
            };
            let s = l.f1().await;
            let z: Vec<&str> = s.collect().await;
            assert_eq!(z, ["qwe", "qwe", "qwe"]);
        });
    }

    #[test]
    fn tokio_join_one_works() {
        futures_executor::block_on(async {
            let stream = fn_stream(|collector| async move {
                tokio::join!(async { collector.emit(1).await },);
                collector.emit(2).await;
            });
            pin_mut!(stream);
            assert_eq!(Some(1), stream.next().await);
            assert_eq!(Some(2), stream.next().await);
            assert_eq!(None, stream.next().await);
        });
    }

    #[test]
    fn tokio_join_many_works() {
        futures_executor::block_on(async {
            let stream = fn_stream(|collector| async move {
                eprintln!("try stream 1");
                tokio::join!(
                    async { collector.emit(1).await },
                    async { collector.emit(2).await },
                    async { collector.emit(3).await },
                );
                collector.emit(4).await;
            });
            pin_mut!(stream);
            for _ in 0..3 {
                let item = stream.next().await;
                assert!(matches!(item, Some(1..=3)));
            }
            assert_eq!(Some(4), stream.next().await);
            assert_eq!(None, stream.next().await);
        });
    }

    #[test]
    fn tokio_futures_unordered_one_works() {
        futures_executor::block_on(async {
            let stream = fn_stream(|collector| async move {
                let mut futs: FuturesUnordered<_> = (1..=1)
                    .map(|i| {
                        let collector = &collector;
                        async move { collector.emit(i).await }
                    })
                    .collect();
                while futs.next().await.is_some() {}
                collector.emit(2).await;
            });
            pin_mut!(stream);
            assert_eq!(Some(1), stream.next().await);
            assert_eq!(Some(2), stream.next().await);
            assert_eq!(None, stream.next().await);
        });
    }

    #[test]
    fn tokio_futures_unordered_many_works() {
        futures_executor::block_on(async {
            let stream = fn_stream(|collector| async move {
                let mut futs: FuturesUnordered<_> = (1..=3)
                    .map(|i| {
                        let collector = &collector;
                        async move { collector.emit(i).await }
                    })
                    .collect();
                while futs.next().await.is_some() {}
                collector.emit(4).await;
            });
            pin_mut!(stream);
            for _ in 1..=3 {
                let item = stream.next().await;
                assert!(matches!(item, Some(1..=3)));
            }
            assert_eq!(Some(4), stream.next().await);
            assert_eq!(None, stream.next().await);
        });
    }
}
