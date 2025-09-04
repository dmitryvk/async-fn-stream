use std::time::Instant;

use async_stream::stream;
use futures_util::{pin_mut, StreamExt};

#[tokio::main]
async fn main() {
    let num_iters: u32 = std::env::var("ASYNC_FN_STREAM_BENCH_ITERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    for _ in 0..num_iters {
        bench_async_fn_stream_sync_numbers().await;
        bench_async_fn_stream_sync_strings().await;
        bench_async_stream_sync_numbers().await;
        bench_async_stream_sync_strings().await;
    }
}

async fn bench_async_fn_stream_sync_numbers() {
    let start = Instant::now();

    let stream = async_fn_stream::fn_stream(|emitter| async move {
        for i in 0..1_000_000 {
            emitter.emit(i).await;
        }
    });
    pin_mut!(stream);
    let mut sum = 0u32;
    while let Some(i) = stream.next().await {
        sum = sum.wrapping_add(i);
    }
    assert!(sum > 0);

    let elapsed = start.elapsed();
    println!("async-fn-stream sync numbers: {} us", elapsed.as_micros());
}

async fn bench_async_fn_stream_sync_strings() {
    let start = Instant::now();

    let stream = async_fn_stream::fn_stream(|emitter| async move {
        for i in 0..1_000_000 {
            emitter.emit(i.to_string()).await;
        }
    });
    pin_mut!(stream);
    let mut sum = 0usize;
    while let Some(i) = stream.next().await {
        sum = sum.wrapping_add(i.len());
    }
    assert!(sum > 0);

    let elapsed = start.elapsed();
    println!("async-fn-stream sync strings: {} us", elapsed.as_micros());
}

async fn bench_async_stream_sync_numbers() {
    let start = Instant::now();

    let stream = stream! {
        for i in 0..1_000_000 {
            yield i;
        }
    };
    pin_mut!(stream);
    let mut sum = 0u32;
    while let Some(i) = stream.next().await {
        sum = sum.wrapping_add(i);
    }
    assert!(sum > 0);

    let elapsed = start.elapsed();
    println!("async-stream sync numbers: {} us", elapsed.as_micros());
}

async fn bench_async_stream_sync_strings() {
    let start = Instant::now();

    let stream = stream! {
        for i in 0..1_000_000 {
            yield i.to_string();
        }
    };
    pin_mut!(stream);
    let mut sum = 0usize;
    while let Some(i) = stream.next().await {
        sum = sum.wrapping_add(i.len());
    }
    assert!(sum > 0);

    let elapsed = start.elapsed();
    println!("async-stream sync strings: {} us", elapsed.as_micros());
}
