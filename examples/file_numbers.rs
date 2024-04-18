use std::env::args;

use anyhow::Context;
use async_fn_stream::try_fn_stream;
use futures_util::{pin_mut, Stream, StreamExt};
use tokio::{
    fs::File,
    io::{AsyncBufReadExt, BufReader},
};

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    let file_name = args().nth(1).unwrap();
    let stream = read_numbers(file_name);
    pin_mut!(stream);
    while let Some(number) = stream.next().await {
        println!("number: {}", number?);
    }

    Ok(())
}

fn read_numbers(file_name: String) -> impl Stream<Item = Result<i32, anyhow::Error>> {
    try_fn_stream(|emitter| async move {
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
                emitter.emit(number).await;
            }
        }

        Ok(())
    })
}
