//! Bounded parallel work whose results retain their input order.

use anyhow::Result;

pub fn workers(items: usize) -> usize {
    let available = std::thread::available_parallelism().map_or(1, |n| n.get());
    let limit = std::env::var("TY_MAX_PARALLELISM")
        .ok()
        .and_then(|value| value.parse::<std::num::NonZeroUsize>().ok())
        .map_or(available, |n| n.get().min(available));
    // Small collections avoid thread startup. Bound large snapshots so a
    // companion check can coexist with the user's other work.
    if items < 128 { 1 } else { limit.min(8) }
}

pub fn map_ordered<T: Sync, R: Send>(
    items: &[T],
    workers: usize,
    operation: impl Fn(&T) -> R + Sync,
) -> Result<Vec<R>> {
    let workers = workers.max(1).min(items.len());
    if workers <= 1 {
        return Ok(items.iter().map(operation).collect());
    }
    std::thread::scope(|scope| {
        let operation = &operation;
        let handles: Vec<_> = items
            .chunks(items.len().div_ceil(workers))
            .map(|chunk| scope.spawn(move || chunk.iter().map(operation).collect::<Vec<_>>()))
            .collect();
        // Join every worker before propagating a panic. Per-item failures stay
        // in input order, including when a later worker finishes first.
        let results: Vec<_> = handles.into_iter().map(|handle| handle.join()).collect();
        let mut output = Vec::with_capacity(items.len());
        for result in results {
            output.extend(result.map_err(|_| anyhow::anyhow!("parallel worker panicked"))?);
        }
        Ok(output)
    })
}

#[cfg(test)]
mod tests {
    use super::map_ordered;

    #[test]
    fn workers_overlap_but_results_keep_input_order() -> anyhow::Result<()> {
        let (finished, receive) = std::sync::mpsc::channel();
        let receive = std::sync::Mutex::new(receive);
        let results = map_ordered(&[0, 1], 2, |item| {
            if *item == 0 {
                receive
                    .lock()
                    .unwrap()
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap();
            } else {
                finished.send(()).unwrap();
            }
            (*item, std::thread::current().id())
        })?;
        assert_eq!([results[0].0, results[1].0], [0, 1]);
        assert_ne!(results[0].1, results[1].1);
        assert!(map_ordered::<u8, u8>(&[], 0, |item| *item)?.is_empty());
        Ok(())
    }
}
