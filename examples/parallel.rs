use async_channel::unbounded;
use easy_parallel::Parallel;

use edge_executor::{block_on, Executor};

fn main() {
    let ex: Executor = Default::default();
    let (signal, shutdown) = unbounded::<()>();

    Parallel::new()
        // Run four executor threads.
        .each(0..4, |_| block_on(ex.run(shutdown.recv())))
        // Run the main future on the current thread.
        .finish(|| {
            block_on(async {
                println!("Hello world!");
                drop(signal);
            })
        });
}
