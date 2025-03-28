#![doc = include_str!("../README.md")]

use std::{
    pin::Pin,
    string::String,
    sync::{Arc, Mutex},
    task::{Poll, Waker},
};

use futures_util::{Future, FutureExt, Stream};
use pin_project_lite::pin_project;

/// An intermediary that transfers values from stream to its consumer
pub struct StreamEmitter<T> {
    inner: Arc<Mutex<Inner<T>>>,
}

/// An intermediary that transfers values from stream to its consumer
pub struct TryStreamEmitter<T, E> {
    inner: Arc<Mutex<Inner<Result<T, E>>>>,
}

struct Inner<T> {
    value: Option<T>,
    waker: Option<Waker>,
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
            value: None,
            waker: None,
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
        let mut this = self.project();

        this.inner.lock().expect("Mutex was poisoned").waker = Some(cx.waker().clone());
        let r = this.fut.poll_unpin(cx);
        match r {
            std::task::Poll::Ready(()) => Poll::Ready(None),
            std::task::Poll::Pending => {
                let value = this.inner.lock().expect("Mutex was poisoned").value.take();
                match value {
                    None => Poll::Pending,
                    Some(value) => Poll::Ready(Some(value)),
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
            value: None,
            waker: None,
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
        if self.is_err {
            return Poll::Ready(None);
        }
        let mut this = self.project();
        this.inner.lock().expect("Mutex was poisoned").waker = Some(cx.waker().clone());
        let r = this.fut.poll_unpin(cx);
        match r {
            std::task::Poll::Ready(Ok(())) => Poll::Ready(None),
            std::task::Poll::Ready(Err(e)) => {
                *this.is_err = true;
                Poll::Ready(Some(Err(e)))
            }
            std::task::Poll::Pending => {
                let value = this.inner.lock().expect("Mutex was poisoned").value.take();
                match value {
                    None => Poll::Pending,
                    Some(value) => Poll::Ready(Some(value)),
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
    pub fn emit(&self, value: T) -> CollectFuture {
        let mut inner = self.inner.lock().expect("Mutex was poisoned");
        let inner = &mut *inner;
        if inner.value.is_some() {
            panic!("StreamEmitter::emit() was called without `.await`'ing result of previous emit")
        }
        inner.value = Some(value);
        inner
            .waker
            .take()
            .expect("StreamEmitter::emit() should only be called in context of Future::poll()")
            .wake();
        CollectFuture { polled: false }
    }
}

impl<T, E> TryStreamEmitter<T, E> {
    fn internal_emit(&self, res: Result<T, E>) -> CollectFuture {
        let mut inner = self.inner.lock().expect("Mutex was poisoned");
        let inner = &mut *inner;
        if inner.value.is_some() {
            panic!(
                "TreStreamEmitter::emit/emit_err() was called without `.await`'ing result of previous collect"
            )
        }
        inner.value = Some(res);
        inner
            .waker
            .take()
            .expect("TreStreamEmitter::emit/emit_err() should only be called in context of Future::poll()")
            .wake();
        CollectFuture { polled: false }
    }

    /// Emit value from a stream and wait until stream consumer calls [`futures_util::StreamExt::next`] again.
    ///
    /// # Panics
    /// Will panic if:
    /// * `emit`/`emit_err` is called twice without awaiting result of the first call
    /// * `emit` is called not in context of polling the stream
    #[must_use = "Ensure that emit() is awaited"]
    pub fn emit(&self, value: T) -> CollectFuture {
        self.internal_emit(Ok(value))
    }

    /// Emit error from a stream and wait until stream consumer calls [`futures_util::StreamExt::next`] again.
    ///
    /// # Panics
    /// Will panic if:
    /// * `emit`/`emit_err` is called twice without awaiting result of the first call
    /// * `emit_err` is called not in context of polling the stream
    #[must_use = "Ensure that emit_err() is awaited"]
    pub fn emit_err(&self, err: E) -> CollectFuture {
        self.internal_emit(Err(err))
    }
}

/// Future returned from [`StreamEmitter::emit`].
pub struct CollectFuture {
    polled: bool,
}

impl Future for CollectFuture {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        if self.polled {
            Poll::Ready(())
        } else {
            self.get_mut().polled = true;
            Poll::Pending
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::ErrorKind;

    use futures_util::{pin_mut, StreamExt};

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
    #[should_panic]
    fn infallible_panics_on_multiple_collects() {
        futures_executor::block_on(async {
            #[allow(unused_must_use)]
            let stream = fn_stream(|collector| async move {
                eprintln!("stream 1");
                collector.emit(1);
                collector.emit(2);
                eprintln!("stream 3");
            });
            pin_mut!(stream);
            assert_eq!(Some(1), stream.next().await);
            assert_eq!(Some(2), stream.next().await);
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
        })
    }
}
