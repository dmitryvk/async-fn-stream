#![doc = include_str!("../README.md")]

use std::{
    cell::{Cell, UnsafeCell},
    pin::Pin,
    sync::Arc,
    task::{Poll, Waker},
};

use futures_util::{Future, Stream};
use pin_project_lite::pin_project;
use smallvec::SmallVec;

/// An intermediary that transfers values from stream to its consumer
pub struct StreamEmitter<T> {
    inner: Arc<UnsafeCell<Inner<T>>>,
}

/// An intermediary that transfers values from stream to its consumer
pub struct TryStreamEmitter<T, E> {
    inner: Arc<UnsafeCell<Inner<Result<T, E>>>>,
}

thread_local! {
    /// A type-erased pointer to the `Inner<_>`, for which a call to `Stream::poll_next` is active.
    static ACTIVE_STREAM_INNER: Cell<*const ()> = const { Cell::new(std::ptr::null()) };
}

/// A guard that ensures that `ACTIVE_STREAM_INNER` is returned to its previous value even in case of a panic.
struct ActiveStreamPointerGuard {
    old_ptr: *const (),
}

impl ActiveStreamPointerGuard {
    fn set_active_ptr(ptr: *const ()) -> Self {
        let old_ptr = ACTIVE_STREAM_INNER.with(|thread_ptr| thread_ptr.replace(ptr));
        Self { old_ptr }
    }
}

impl Drop for ActiveStreamPointerGuard {
    fn drop(&mut self) {
        ACTIVE_STREAM_INNER.with(|thread_ptr| thread_ptr.set(self.old_ptr));
    }
}

/// SAFETY:
/// `Inner<T>` stores the state shared between `StreamEmitter`/`TryStreamEmitter` and `FnStream`/`TryFnStream`.
/// It is stored within `Arc<UnsafeCell<_>>`. Exclusive access to it is controlled using:
/// 1) exclusive reference `Pin<&mut FnStream>` which is provided to `FnStream::poll_next`
/// 2) `ACTIVE_STREAM_INNER`, which is itself managed by `FnStream::poll_next`.
///    - `ACTIVE_STREAM_INNER` is only set when the corresponding `Pin<&mut FnStream>` is on the stack
///    - `EmitFuture` can safely access its inner reference if it equals to `ACTIVE_STREAM_INNER` (all other accesses are invalid and lead to panics)
struct Inner<T> {
    // `stream_waker` is used to compare the waker of the stream future with the waker of the `Emit` future.
    // If the stream implementation does not have sub-executors, we don't have to push wakers to `pending_wakers`, which avoids cloning it.
    stream_waker: Option<Waker>,
    // Due to internal concurrency, a single call to stream's future may yield multiple elements.
    // All elements are stored here and yielded from the stream before polling the future again.
    pending_values: SmallVec<[T; 1]>,
    pending_wakers: SmallVec<[Waker; 1]>,
}

/// SAFETY: `FnStream` implements own synchronization
unsafe impl<T: Send, Fut: Future<Output = ()> + Send> Send for FnStream<T, Fut> {}
/// SAFETY: `TryFnStream` implements own synchronization
unsafe impl<T: Send, E: Send, Fut: Future<Output = Result<(), E>> + Send> Send
    for TryFnStream<T, E, Fut>
{
}
/// SAFETY: `StreamEmitter` implements own synchronization
unsafe impl<T: Send> Send for StreamEmitter<T> {}
/// SAFETY: `TryStreamEmitter` implements own synchronization
unsafe impl<T: Send, E: Send> Send for TryStreamEmitter<T, E> {}
/// SAFETY: `FnStream` implements own synchronization
unsafe impl<T: Send, Fut: Future<Output = ()> + Send> Sync for FnStream<T, Fut> {}
/// SAFETY: `TryFnStream` implements own synchronization
unsafe impl<T: Send, E: Send, Fut: Future<Output = Result<(), E>> + Send> Sync
    for TryFnStream<T, E, Fut>
{
}
/// SAFETY: `StreamEmitter` implements own synchronization
unsafe impl<T: Send> Sync for StreamEmitter<T> {}
/// SAFETY: `TryStreamEmitter` implements own synchronization
unsafe impl<T: Send, E: Send> Sync for TryStreamEmitter<T, E> {}

pin_project! {
    /// Implementation of [`Stream`] trait created by [`fn_stream`].
    pub struct FnStream<T, Fut: Future<Output = ()>> {
        #[pin]
        fut: Fut,
        inner: Arc<UnsafeCell<Inner<T>>>,
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
        let inner = Arc::new(UnsafeCell::new(Inner {
            stream_waker: None,
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

        // SAFETY:
        // (see safety comment for `Inner<T>`)
        // 1) we have no aliasing, since we're holding unique reference to `Self`, and
        // 2) `this.inner` is not deallocated for the duration of this method
        let inner = unsafe { &mut *this.inner.get() };
        if let Some(value) = inner.pending_values.pop() {
            return Poll::Ready(Some(value));
        }
        if !inner.pending_wakers.is_empty() {
            for waker in inner.pending_wakers.drain(..) {
                if !waker.will_wake(cx.waker()) {
                    waker.wake();
                }
            }
        }
        if let Some(stream_waker) = inner.stream_waker.as_mut() {
            stream_waker.clone_from(cx.waker());
        } else {
            inner.stream_waker = Some(cx.waker().clone());
        }

        // SAFETY: ensure that we're not holding a reference to this.inner
        _ = inner;

        // SAFETY for Inner<T>:
        // - `ACTIVE_STREAM_INNER` now contains valid pointer to `this.inner` during the call to `fut.poll`
        // - `ACTIVE_STREAM_INNER` is restored after the call due to the use of guard
        let polling_ptr_guard =
            ActiveStreamPointerGuard::set_active_ptr(Arc::as_ptr(&*this.inner).cast());
        let r = this.fut.poll(cx);
        drop(polling_ptr_guard);

        // SAFETY:
        // (see safety comment for `Inner<T>`)
        // 1) we have no aliasing, since:
        //    - we're holding unique reference to `Self`, and
        //    - we removed the pointer from `ACTIVE_STREAM_INNER`
        // 2) `this.inner` is not deallocated for the duration of this method
        let inner = unsafe { &mut *this.inner.get() };

        match r {
            std::task::Poll::Ready(()) => Poll::Ready(None),
            std::task::Poll::Pending => {
                if let Some(value) = inner.pending_values.pop() {
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
/// - return successive stream elements via [`TryStreamEmitter::emit`]
/// - return transient errors via [`TryStreamEmitter::emit_err`]
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
        inner: Arc<UnsafeCell<Inner<Result<T, E>>>>,
    }
}

impl<T, E, Fut: Future<Output = Result<(), E>>> TryFnStream<T, E, Fut> {
    fn new<F: FnOnce(TryStreamEmitter<T, E>) -> Fut>(func: F) -> Self {
        let inner = Arc::new(UnsafeCell::new(Inner {
            stream_waker: None,
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
        // SAFETY:
        // (see safety comment for `Inner<T>`)
        // 1) we have no aliasing, since we're holding unique reference to `Self`, and
        // 2) `this.inner` is not deallocated for the duration of this method
        let inner = unsafe { &mut *this.inner.get() };
        if let Some(value) = inner.pending_values.pop() {
            return Poll::Ready(Some(value));
        }
        if !inner.pending_wakers.is_empty() {
            for waker in inner.pending_wakers.drain(..) {
                if !waker.will_wake(cx.waker()) {
                    waker.wake();
                }
            }
        }
        if let Some(stream_waker) = inner.stream_waker.as_mut() {
            stream_waker.clone_from(cx.waker());
        } else {
            inner.stream_waker = Some(cx.waker().clone());
        }

        // SAFETY: ensure that we're not holding a reference to this.inner
        _ = inner;

        // SAFETY for Inner<T>:
        // - `ACTIVE_STREAM_INNER` now contains valid pointer to `this.inner` during the call to `fut.poll`
        // - `ACTIVE_STREAM_INNER` is restored after the call due to the use of guard
        let polling_ptr_guard =
            ActiveStreamPointerGuard::set_active_ptr(Arc::as_ptr(&*this.inner).cast());
        let r = this.fut.poll(cx);
        drop(polling_ptr_guard);

        // SAFETY:
        // (see safety comment for `Inner<T>`)
        // 1) we have no aliasing, since:
        //    - we're holding unique reference to `Self`, and
        //    - we removed the pointer from `ACTIVE_STREAM_INNER`
        // 2) `this.inner` is not deallocated for the duration of this method
        let inner = unsafe { &mut *this.inner.get() };
        match r {
            std::task::Poll::Ready(Ok(())) => Poll::Ready(None),
            std::task::Poll::Ready(Err(e)) => {
                *this.is_err = true;
                Poll::Ready(Some(Err(e)))
            }
            std::task::Poll::Pending => {
                if let Some(value) = inner.pending_values.pop() {
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
    pub fn emit(&'_ self, value: T) -> EmitFuture<'_, T> {
        EmitFuture::new(&self.inner, value)
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
    pub fn emit(&'_ self, value: T) -> EmitFuture<'_, Result<T, E>> {
        EmitFuture::new(&self.inner, Ok(value))
    }

    /// Emit error from a stream and wait until stream consumer calls [`futures_util::StreamExt::next`] again.
    ///
    /// # Panics
    /// Will panic if:
    /// * `emit`/`emit_err` is called twice without awaiting result of the first call
    /// * `emit_err` is called not in context of polling the stream
    #[must_use = "Ensure that emit_err() is awaited"]
    pub fn emit_err(&'_ self, err: E) -> EmitFuture<'_, Result<T, E>> {
        EmitFuture::new(&self.inner, Err(err))
    }
}

pin_project! {
    /// Future returned from [`StreamEmitter::emit`].
    pub struct EmitFuture<'a, T> {
        inner: &'a UnsafeCell<Inner<T>>,
        value: Option<T>,
    }
}

impl<'a, T> EmitFuture<'a, T> {
    fn new(inner: &'a UnsafeCell<Inner<T>>, value: T) -> Self {
        Self {
            inner,
            value: Some(value),
        }
    }
}

impl<T> Future for EmitFuture<'_, T> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        assert!(
            ACTIVE_STREAM_INNER.get() == std::ptr::from_ref(*this.inner).cast::<()>(),
            "StreamEmitter::emit().await should only be called in the context of the corresponding `fn_stream()`/`try_fn_stream()`"
        );
        // SAFETY:
        // 1) we hold a unique reference to `this.inner` since we verified that ACTIVE_STREAM_INNER == self.inner
        //    - we're calling `{Try,}FnStream::poll_next` in this thread, which holds the only other reference to the same instance of `Inner<T>`
        //    - `{Try,}FnStream::poll_next` is not holding a reference to `self.inner` during the call to `fut.poll`
        // 2) `this.inner` is not deallocated for the duration of this method
        let inner = unsafe { &mut *this.inner.get() };

        if let Some(value) = this.value.take() {
            inner.pending_values.push(value);
            let is_same_waker = if let Some(stream_waker) = inner.stream_waker.as_ref() {
                stream_waker.will_wake(cx.waker())
            } else {
                false
            };
            if !is_same_waker {
                inner.pending_wakers.push(cx.waker().clone());
            }
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
    use std::{io::ErrorKind, pin::pin};

    use futures_util::{pin_mut, stream::FuturesUnordered, StreamExt};

    use super::*;

    #[test]
    fn infallible_works() {
        futures_executor::block_on(async {
            let stream = fn_stream(|emitter| async move {
                eprintln!("stream 1");
                emitter.emit(1).await;
                eprintln!("stream 2");
                emitter.emit(2).await;
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
            let stream = fn_stream(|emitter| async move {
                eprintln!("stream 1");
                emitter.emit(a).await;
                eprintln!("stream 2");
                emitter.emit(b).await;
                eprintln!("stream 3");
            });
            pin_mut!(stream);
            assert_eq!(Some(a), stream.next().await);
            assert_eq!(Some(b), stream.next().await);
            assert_eq!(None, stream.next().await);
        });
    }

    #[test]
    fn infallible_unawaited_emit_is_ignored() {
        futures_executor::block_on(async {
            #[expect(
                unused_must_use,
                reason = "this code intentionally does not await emitter.emit()"
            )]
            let stream = fn_stream(|emitter| async move {
                emitter.emit(1)/* .await */;
                emitter.emit(2)/* .await */;
                emitter.emit(3).await;
            });
            pin_mut!(stream);
            assert_eq!(Some(3), stream.next().await);
            assert_eq!(None, stream.next().await);
        });
    }

    #[test]
    fn fallible_works() {
        futures_executor::block_on(async {
            let stream = try_fn_stream(|emitter| async move {
                eprintln!("try stream 1");
                emitter.emit(1).await;
                eprintln!("try stream 2");
                emitter.emit(2).await;
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
            let stream = try_fn_stream(|emitter| async move {
                eprintln!("try stream 1");
                emitter.emit(1).await;
                eprintln!("try stream 2");
                emitter.emit(2).await;
                eprintln!("try stream 3");
                emitter
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
                fn_stream(|emitter| async move {
                    emitter.emit(self.a.as_str()).await;
                    emitter.emit(self.a.as_str()).await;
                    emitter.emit(self.a.as_str()).await;
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
            let stream = fn_stream(|emitter| async move {
                tokio::join!(async { emitter.emit(1).await },);
                emitter.emit(2).await;
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
            let stream = fn_stream(|emitter| async move {
                eprintln!("try stream 1");
                tokio::join!(
                    async { emitter.emit(1).await },
                    async { emitter.emit(2).await },
                    async { emitter.emit(3).await },
                );
                emitter.emit(4).await;
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
            let stream = fn_stream(|emitter| async move {
                let mut futs: FuturesUnordered<_> = (1..=1)
                    .map(|i| {
                        let emitter = &emitter;
                        async move { emitter.emit(i).await }
                    })
                    .collect();
                while futs.next().await.is_some() {}
                emitter.emit(2).await;
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
            let stream = fn_stream(|emitter| async move {
                let mut futs: FuturesUnordered<_> = (1..=3)
                    .map(|i| {
                        let emitter = &emitter;
                        async move { emitter.emit(i).await }
                    })
                    .collect();
                while futs.next().await.is_some() {}
                emitter.emit(4).await;
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

    #[test]
    fn infallible_nested_streams_work() {
        futures_executor::block_on(async {
            let mut stream = pin!(fn_stream(|emitter| async move {
                for i in 0..3 {
                    let mut stream_2 = pin!(fn_stream(|emitter| async move {
                        for j in 0..3 {
                            emitter.emit(j).await;
                        }
                    }));
                    while let Some(item) = stream_2.next().await {
                        emitter.emit(3 * i + item).await;
                    }
                }
            }));
            let mut sum = 0;
            while let Some(item) = stream.next().await {
                sum += item;
            }
            assert_eq!(sum, 36);
        });
    }

    #[test]
    fn fallible_nested_streams_work() {
        futures_executor::block_on(async {
            let mut stream = pin!(try_fn_stream(|emitter| async move {
                for i in 0..3 {
                    let mut stream_2 = pin!(try_fn_stream(|emitter| async move {
                        for j in 0..3 {
                            emitter.emit(j).await;
                        }
                        Ok::<_, ()>(())
                    }));
                    while let Some(Ok(item)) = stream_2.next().await {
                        emitter.emit(3 * i + item).await;
                    }
                }
                Ok::<_, ()>(())
            }));
            let mut sum = 0;
            while let Some(Ok(item)) = stream.next().await {
                sum += item;
            }
            assert_eq!(sum, 36);
        });
    }

    #[test]
    #[should_panic(
        expected = "StreamEmitter::emit().await should only be called in the context of the corresponding `fn_stream()`/`try_fn_stream()`"
    )]
    fn infallible_bad_nested_emit_detected() {
        futures_executor::block_on(async {
            let mut stream = pin!(fn_stream(|emitter| async move {
                for i in 0..3 {
                    let emitter_ref = &emitter;
                    let mut stream_2 = pin!(fn_stream(|emitter_2| async move {
                        emitter_2.emit(0).await;
                        for j in 0..3 {
                            emitter_ref.emit(j).await;
                        }
                    }));
                    while let Some(item) = stream_2.next().await {
                        emitter.emit(3 * i + item).await;
                    }
                }
            }));

            let mut sum = 0;
            while let Some(item) = stream.next().await {
                sum += item;
            }
            assert_eq!(sum, 36);
        });
    }

    #[test]
    #[should_panic(
        expected = "StreamEmitter::emit().await should only be called in the context of the corresponding `fn_stream()`/`try_fn_stream()`"
    )]
    fn fallible_bad_nested_emit_detected() {
        futures_executor::block_on(async {
            let mut stream = pin!(try_fn_stream(|emitter| async move {
                for i in 0..3 {
                    let emitter_ref = &emitter;
                    let mut stream_2 = pin!(try_fn_stream(|emitter_2| async move {
                        emitter_2.emit(0).await;
                        for j in 0..3 {
                            emitter_ref.emit(j).await;
                        }
                        Ok::<_, ()>(())
                    }));
                    while let Some(Ok(item)) = stream_2.next().await {
                        emitter.emit(3 * i + item).await;
                    }
                }
                Ok::<_, ()>(())
            }));

            let mut sum = 0;
            while let Some(Ok(item)) = stream.next().await {
                sum += item;
            }
            assert_eq!(sum, 36);
        });
    }
}
