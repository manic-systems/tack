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

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{
            AtomicUsize,
            Ordering,
        },
    };

    use super::{
        ordered,
        stream,
    };

    #[test]
    fn ordered_preserves_input_order() {
        let out = ordered(vec![3_i32, 2_i32, 1_i32], 2, |_, item| item * 2_i32);

        assert_eq!(out, vec![6_i32, 4_i32, 2_i32]);
    }

    #[test]
    fn stream_reports_original_indices() {
        let mut seen = Vec::new();
        stream(
            vec!["a", "b", "c"],
            2,
            |_, item| item.to_owned(),
            |index, value| {
                seen.push((index, value));
            },
        );
        seen.sort_by_key(|entry| entry.0);

        assert_eq!(seen, vec![
            (0, "a".to_owned()),
            (1, "b".to_owned()),
            (2, "c".to_owned())
        ]);
    }

    #[test]
    fn stream_respects_in_flight_limit() {
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let active_for_run = Arc::clone(&active);
        let peak_for_run = Arc::clone(&peak);

        stream(
            (0_i32..8_i32).collect::<Vec<_>>(),
            3,
            move |_, item| {
                let now = active_for_run.fetch_add(1, Ordering::SeqCst) + 1;
                peak_for_run.fetch_max(now, Ordering::SeqCst);
                active_for_run.fetch_sub(1, Ordering::SeqCst);
                item
            },
            |_, _| {},
        );

        assert!(peak.load(Ordering::SeqCst) <= 3);
    }
}
