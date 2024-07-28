use std::async_iter::AsyncIterator;

use crate::{FnStream, TryFnStream};
use core::future::Future;
use futures_util::Stream;

impl<T, Fut: Future<Output = ()>> AsyncIterator for FnStream<T, Fut> {
    type Item = <Self as Stream>::Item;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        Stream::poll_next(self, cx)
    }
}

impl<T, E, Fut: Future<Output = Result<(), E>>> AsyncIterator for TryFnStream<T, E, Fut> {
    type Item = <Self as Stream>::Item;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        Stream::poll_next(self, cx)
    }
}

// #[cfg(test)]
// mod tests {
//     use std::io::ErrorKind;

//     use futures_util::pin_mut;

//     use crate::{fn_stream, try_fn_stream};

//     #[test]
//     fn infallible_works() {
//         futures_executor::block_on(async {
//             let stream = fn_stream(|collector| async move {
//                 eprintln!("stream 1");
//                 collector.emit(1).await;
//                 eprintln!("stream 2");
//                 collector.emit(2).await;
//                 eprintln!("stream 3");
//             });
//             pin_mut!(stream);
//             assert_eq!(Some(1), stream.next().await);
//             assert_eq!(Some(2), stream.next().await);
//             assert_eq!(None, stream.next().await);
//         });
//     }

//     #[test]
//     fn fallible_works() {
//         futures_executor::block_on(async {
//             let stream = try_fn_stream(|collector| async move {
//                 eprintln!("try stream 1");
//                 collector.emit(1).await;
//                 eprintln!("try stream 2");
//                 collector.emit(2).await;
//                 eprintln!("try stream 3");
//                 Err(std::io::Error::from(ErrorKind::Other))
//             });
//             pin_mut!(stream);
//             assert_eq!(1, stream.next().await.unwrap().unwrap());
//             assert_eq!(2, stream.next().await.unwrap().unwrap());
//             assert!(stream.next().await.unwrap().is_err());
//             assert!(stream.next().await.is_none());
//         });
//     }
// }
