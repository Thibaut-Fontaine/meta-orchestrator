//! CPU-aware scheduling: each running agent gets its own set of cores, and the
//! number of simultaneous agents is bounded.

/// Partition the machine's cores between agents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scheduler {
    total_cores: usize,
    reserve_cores: usize,
    max_parallel: usize,
}

impl Scheduler {
    pub fn new(total_cores: usize, reserve_cores: usize, max_parallel: usize) -> Self {
        let total = total_cores.max(1);
        let reserve = reserve_cores.min(total.saturating_sub(1));
        let usable = total - reserve;
        let max_parallel = max_parallel.max(1).min(usable);
        Scheduler {
            total_cores: total,
            reserve_cores: reserve,
            max_parallel,
        }
    }

    pub fn total_cores(&self) -> usize {
        self.total_cores
    }

    pub fn reserve_cores(&self) -> usize {
        self.reserve_cores
    }

    pub fn max_parallel(&self) -> usize {
        self.max_parallel
    }

    pub fn usable_cores(&self) -> usize {
        self.total_cores - self.reserve_cores
    }

    /// Split usable cores into contiguous, disjoint groups (one per parallel
    /// slot). Lower-numbered slots receive the remainder when it does not divide
    /// evenly.
    pub fn slots(&self) -> Vec<Vec<usize>> {
        let usable = self.usable_cores();
        let base = usable / self.max_parallel;
        let remainder = usable % self.max_parallel;
        let mut next = self.reserve_cores;
        let mut slots = Vec::with_capacity(self.max_parallel);
        for slot in 0..self.max_parallel {
            let size = base + usize::from(slot < remainder);
            let cores = (next..next + size).collect();
            next += size;
            slots.push(cores);
        }
        slots
    }

    /// Cores for the n-th concurrently running agent (wraps when more agents are
    /// launched than slots; the caller normally batches first).
    pub fn cores_for(&self, slot: usize) -> Vec<usize> {
        let slots = self.slots();
        slots[slot % slots.len()].clone()
    }

    /// Indices of projects grouped into sequential waves of `max_parallel`.
    pub fn batches(&self, project_count: usize) -> Vec<Vec<usize>> {
        (0..project_count)
            .collect::<Vec<_>>()
            .chunks(self.max_parallel)
            .map(|chunk| chunk.to_vec())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eight_cores_leave_one_free_and_two_agents() {
        let s = Scheduler::new(8, 1, 2);
        assert_eq!(s.usable_cores(), 7);
        assert_eq!(s.max_parallel(), 2);
        assert_eq!(s.slots(), vec![vec![1, 2, 3, 4], vec![5, 6, 7]]);
    }

    #[test]
    fn slots_are_disjoint_and_never_use_reserved_cores() {
        let s = Scheduler::new(6, 2, 4);
        let slots = s.slots();
        let mut all: Vec<usize> = slots.iter().flatten().copied().collect();
        assert!(all.iter().all(|c| *c >= 2 && *c < 6));
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), 4);
    }

    #[test]
    fn clamps_parallel_to_usable_cores() {
        let s = Scheduler::new(2, 1, 10);
        assert_eq!(s.max_parallel(), 1);
        assert_eq!(s.slots(), vec![vec![1]]);
    }

    #[test]
    fn single_core_machine_still_works() {
        let s = Scheduler::new(1, 1, 4);
        assert_eq!(s.max_parallel(), 1);
        assert_eq!(s.slots(), vec![vec![0]]);
        assert_eq!(s.usable_cores(), 1);
    }

    #[test]
    fn batches_group_projects() {
        let s = Scheduler::new(8, 1, 3);
        assert_eq!(s.batches(7), vec![vec![0, 1, 2], vec![3, 4, 5], vec![6]]);
        assert!(s.batches(0).is_empty());
    }

    #[test]
    fn cores_for_wraps() {
        let s = Scheduler::new(4, 0, 2);
        assert_eq!(s.cores_for(0), vec![0, 1]);
        assert_eq!(s.cores_for(1), vec![2, 3]);
        assert_eq!(s.cores_for(2), vec![0, 1]);
    }
}
