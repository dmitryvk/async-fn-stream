A version of [async-stream](https://github.com/tokio-rs/async-stream) without macros.
This crate provides generic implementations of `Stream` trait.
`Stream` is an asynchronous version of `std::iter::Iterator`.

Two functions are provided - `fn_stream` and `try_fn_stream`.

# Usage

If you need to create a stream that may result in error, use `try_fn_stream`, otherwise use `fn_stream`.

To create a stream:

1.  Invoke `fn_stream` or `try_fn_stream`, passing a closure (anonymous function).
2.  Closure will accept an `emitter`.
    To return value from the stream, call `.emit(value)` on `emitter` and `.await` on its result.
    Once stream consumer has processed the value and called `.next()` on stream, `.await` will return.
3.  (for `try_fn_stream` only) Return errors from closure via `return Err(...)` or `?` (question mark) operator.

# Examples

Finite stream of numbers

```rust
use async_fn_stream::fn_stream;
use futures::Stream;

fn build_stream() -> impl Stream<Item = i32> {
    fn_stream(|emitter| async move {
        for i in 0..3 {
            // yield elements from stream via `emitter`
            emitter.emit(i).await;
        }
    })
}
```

Read numbers from text file, with error handling

```rust
use anyhow::Context;
use async_fn_stream::try_fn_stream;
use futures::{pin_mut, Stream, StreamExt};
use tokio::{
    fs::File,
    io::{AsyncBufReadExt, BufReader},
};

fn read_numbers(file_name: String) -> impl Stream<Item = Result<i32, anyhow::Error>> {
    try_fn_stream(|emitter| async move {
        // Return errors via `?` operator.
        let file = BufReader::new(File::open(file_name).await.context("Failed to open file")?);
        pin_mut!(file);
        let mut line = String::new();
        loop {
            line.clear();
            let byte_count = file
                .read_line(&mut line)
                .await
                .context("Failed to read line")?;
            if byte_count == 0 {
                break;
            }

            for token in line.split_ascii_whitespace() {
                let number: i32 = token
                    .parse()
                    .with_context(|| format!("Failed to conver string \"{token}\" to number"))?;
                // Return errors via `?` operator.
                emitter.emit(number).await;
            }
        }

        Ok(())
    })
}
```

# Why not `async-stream`?

[async-stream](https://github.com/tokio-rs/async-stream) is great!
It has a nice syntax, but it is based on macros which brings some flaws:
* proc-macros sometimes interacts badly with IDEs such as rust-analyzer or IntelliJ Rust.
  see e.g. <https://github.com/rust-lang/rust-analyzer/issues/11533>
* proc-macros may increase build times
