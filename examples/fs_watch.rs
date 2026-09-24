use notify::Watcher;
use std::sync::mpsc::channel;
use std::time::Duration;

fn main() {
    let (tx, rx) = channel();
    let mut watcher = notify::recommended_watcher(move |res| {
        let _ = tx.send(res);
    })
    .expect("watcher");
    watcher
        .watch(
            std::path::Path::new("tmp/checks"),
            notify::RecursiveMode::NonRecursive,
        )
        .expect("watch");
    println!("watching, touch a file in tmp/checks...");
    for _ in 0..10 {
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(res) => println!("EVENT: {res:?}"),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => println!("(1s, nothing)"),
            Err(e) => {
                println!("recv err: {e}");
                break;
            }
        }
    }
}
