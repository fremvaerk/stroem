//! Tokio runtime sizing. `#[tokio::main]` sizes from `available_parallelism()`,
//! which honours the cgroup CPU quota — a `500m` limit yields ONE worker
//! thread (spec § 5). Floor it so the process is not at the mercy of the
//! deployed quota.

/// Minimum tokio worker threads, whatever the CPU quota says.
pub const MIN_WORKER_THREADS: usize = 4;

/// Worker threads for the server runtime: `max(MIN_WORKER_THREADS, available)`.
pub fn worker_threads(available: Option<usize>) -> usize {
    available.unwrap_or(1).max(MIN_WORKER_THREADS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floors_unknown_parallelism() {
        assert_eq!(worker_threads(None), MIN_WORKER_THREADS);
    }

    #[test]
    fn floors_a_sub_core_quota() {
        // 500m quota → available_parallelism() == 1
        assert_eq!(worker_threads(Some(1)), 4);
        assert_eq!(worker_threads(Some(2)), 4);
    }

    #[test]
    fn keeps_larger_parallelism() {
        assert_eq!(worker_threads(Some(16)), 16);
    }
}
