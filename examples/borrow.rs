use edge_executor::{block_on, LocalExecutor};

fn main() {
    let local_ex: LocalExecutor = Default::default();

    // Borrowed by `&mut` inside the future spawned on the executor
    let mut data = 3;

    let data = &mut data;

    let task = local_ex.spawn(async move {
        *data += 1;

        *data
    });

    let res = block_on(local_ex.run(async { task.await * 2 }));

    assert_eq!(res, 8);
}
