use async_fn_stream::fn_stream;
use futures_util::{pin_mut, Stream, StreamExt};

fn build_stream() -> impl Stream<Item = i32> {
    fn_stream(|emitter| async move {
        tokio::join!(
            async {
                for i in 0..3 {
                    // yield elements from stream via `emitter`
                    emitter.emit(i).await;
                }
            },
            async {
                for i in 10..13 {
                    // yield elements from stream via `emitter`
                    emitter.emit(i).await;
                }
            }
        );
    })
}

async fn example() {
    let stream = build_stream();

    pin_mut!(stream);
    let mut numbers = Vec::new();
    while let Some(number) = stream.next().await {
        print!("{number} ");
        numbers.push(number);
    }
    println!();
    numbers.sort_unstable();
    assert_eq!(numbers, vec![0, 1, 2, 10, 11, 12]);
}

#[tokio::main]
async fn main() {
    futures_executor::block_on(example());
}
