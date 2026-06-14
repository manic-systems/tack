// SPDX-License-Identifier: EUPL-1.2

use std::{
    sync::mpsc,
    thread,
};

pub fn stream<I, O, F, G>(items: Vec<I>, limit: usize, run: F, mut on_result: G)
where
    I: Send,
    O: Send,
    F: Fn(usize, I) -> O + Sync,
    G: FnMut(usize, O),
{
    let effective_limit = limit.max(1);
    thread::scope(|scope| {
        let (tx, rx) = mpsc::channel();
        let mut pending = items.into_iter().enumerate();
        let mut in_flight = 0_usize;
        let mut sender = Some(tx);

        while in_flight < effective_limit {
            let Some((index, item)) = pending.next() else {
                break;
            };
            spawn(scope, sender.as_ref().unwrap(), &run, index, item);
            in_flight += 1;
        }
        if pending.len() == 0 {
            sender.take();
        }

        while in_flight > 0 {
            let Ok((index, output)) = rx.recv() else {
                break;
            };
            in_flight -= 1;
            on_result(index, output);

            if let Some((next_index, item)) = pending.next() {
                spawn(scope, sender.as_ref().unwrap(), &run, next_index, item);
                if pending.len() == 0 {
                    sender.take();
                }
                in_flight += 1;
            } else {
                sender.take();
            }
        }
    });
}

pub fn ordered<I, O, F>(items: Vec<I>, limit: usize, run: F) -> Vec<O>
where
    I: Send,
    O: Send,
    F: Fn(usize, I) -> O + Sync,
{
    let mut outputs = Vec::new();
    stream(items, limit, run, |index, output| {
        outputs.push((index, output));
    });
    outputs.sort_by_key(|entry| entry.0);
    outputs
        .into_iter()
        .map(|(_, output)| output)
        .collect::<Vec<_>>()
}

fn spawn<'scope, I, O, F>(
    scope: &'scope thread::Scope<'scope, '_>,
    tx: &mpsc::Sender<(usize, O)>,
    run: &'scope F,
    index: usize,
    item: I,
) where
    I: Send + 'scope,
    O: Send + 'scope,
    F: Fn(usize, I) -> O + Sync + 'scope,
{
    let sender = tx.clone();
    scope.spawn(move || {
        let output = run(index, item);
        _ = sender.send((index, output));
    });
}
