//! A version of [async-stream](https://github.com/tokio-rs/async-stream) without macros.
//! This crate provides generic implementations of [`Stream`] trait.
//! [`Stream`] is an asynchronous version of [`std::iter::Iterator`].
//!
//! Two functions are provided - [`fn_stream`] and [`try_fn_stream`].
//!
//! # Usage
//!
//! If you need to create a stream that may result in error, use [`try_fn_stream`], otherwise use [`fn_stream`].
//!
//! To create a stream:
//!
//! 1.  Invoke [`fn_stream`] or [`try_fn_stream`], passing a closure (anonymous function).
//! 2.  Closure will accept an `emitter`.
//!     To return value from the stream, call `.emit(value)` on `emitter` and `.await` on its result.
//!     Once stream consumer has processed the value and called `.next()` on stream, `.await` will return.
//! 3.  (for [`try_fn_stream`] only) Return errors from closure via `return Err(...)` or `?` (question mark) operator.
//!
//! # Examples
//!
//! Finite stream of numbers
//!
//! ```rust
//! use async_fn_stream::fn_stream;
//! use futures_util::Stream;
//!
//! fn build_stream() -> impl Stream<Item = i32> {
//!     fn_stream(|emitter| async move {
//!         for i in 0..3 {
//!             // yield elements from stream via `emitter`
//!             emitter.emit(i).await;
//!         }
//!     })
//! }
//! ```
//!
//! Read numbers from text file, with error handling
//!
//! ```rust
//! use anyhow::Context;
//! use async_fn_stream::try_fn_stream;
//! use futures_util::{pin_mut, Stream, StreamExt};
//! use tokio::{
//!     fs::File,
//!     io::{AsyncBufReadExt, BufReader},
//! };
//!
//! fn read_numbers(file_name: String) -> impl Stream<Item = Result<i32, anyhow::Error>> {
//!     try_fn_stream(|emitter| async move {
//!         // Return errors via `?` operator.
//!         let file = BufReader::new(File::open(file_name).await.context("Failed to open file")?);
//!         pin_mut!(file);
//!         let mut line = String::new();
//!         loop {
//!             line.clear();
//!             let byte_count = file
//!                 .read_line(&mut line)
//!                 .await
//!                 .context("Failed to read line")?;
//!             if byte_count == 0 {
//!                 break;
//!             }
//!
//!             for token in line.split_ascii_whitespace() {
//!                 let number: i32 = token
//!                     .parse()
//!                     .with_context(|| format!("Failed to conver string \"{token}\" to number"))?;
//!                 // Return errors via `?` operator.
//!                 emitter.emit(number).await;
//!             }
//!         }
//!
//!         Ok(())
//!     })
//! }
//! ```
//!
//! # Why not `async-stream`?
//!
//! [async-stream](https://github.com/tokio-rs/async-stream) is great!
//! It has a nice syntax, but it is based on macros which brings some flaws:
//! * proc-macros sometimes interacts badly with IDEs such as rust-analyzer or IntelliJ Rust.
//!   see e.g. <https://github.com/rust-lang/rust-analyzer/issues/11533>
//! * proc-macros may increase build times

use std::{
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Poll, Waker},
};

use futures_util::{Future, FutureExt, Stream};
use pin_project_lite::pin_project;

/// An intemediary that transfers values from stream to its consumer
pub struct StreamEmitter<T> {
    inner: Arc<Mutex<Inner<T>>>,
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
        let collector = StreamEmitter {
            inner: inner.clone(),
        };
        let fut = func(collector);
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
/// Caller should pass an async function which will return successive stream elements via [`StreamEmitter::emit`] or returns errors as [`Result::Err`].
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
///         // return errors as `Result::Err`
///         Err(anyhow::anyhow!("An error happened"))
///     })
/// }
/// ```
pub fn try_fn_stream<T, E, Fut: Future<Output = Result<(), E>>>(
    func: impl FnOnce(StreamEmitter<T>) -> Fut,
) -> TryFnStream<T, E, Fut> {
    TryFnStream::new(func)
}

pin_project! {
    /// Implementation of [`Stream`] trait created by [`try_fn_stream`].
    pub struct TryFnStream<T, E, Fut: Future<Output = Result<(), E>>> {
        is_err: bool,
        #[pin]
        fut: Fut,
        inner: Arc<Mutex<Inner<T>>>,
    }
}

impl<T, E, Fut: Future<Output = Result<(), E>>> TryFnStream<T, E, Fut> {
    fn new<F: FnOnce(StreamEmitter<T>) -> Fut>(func: F) -> Self {
        let inner = Arc::new(Mutex::new(Inner {
            value: None,
            waker: None,
        }));
        let collector = StreamEmitter {
            inner: inner.clone(),
        };
        let fut = func(collector);
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
                    Some(value) => Poll::Ready(Some(Ok(value))),
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
    /// * `collect` is called twice without awaiting result of first call
    /// * `collect` is called not in context of polling the stream
    #[must_use = "Ensure that collect() is awaited"]
    pub fn emit(&self, value: T) -> CollectFuture {
        let mut inner = self.inner.lock().expect("Mutex was poisoned");
        let inner = &mut *inner;
        if inner.value.is_some() {
            panic!(
                "Collector::collect() was called without `.await`'ing result of previous collect"
            )
        }
        inner.value = Some(value);
        inner
            .waker
            .take()
            .expect("Collector::collect() should only be called in context of Future::poll()")
            .wake();
        CollectFuture { polled: false }
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
            assert!(stream.next().await.unwrap().err().is_some());
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
