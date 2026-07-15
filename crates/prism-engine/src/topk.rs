//! The bounded nearest-`cap` selection (S6) — the "top-k" the determinism contract names.
//!
//! Extracted from the scan so it can be exercised directly under a counting allocator: the
//! contract requires the block scan and this top-k to perform **zero heap allocations** across a
//! full golden run, after their buffers are sized once ([docs/DETERMINISM-CONTRACT.md](../../../docs/DETERMINISM-CONTRACT.md) §4).
//!
//! It is an explicit binary max-heap rather than `BinaryHeap<T>` for one reason: its comparator
//! must reach **outside** the element — into the per-part scalar columns — for the `event_id`
//! tie-break, so that a candidate can be a plain `Copy` struct of indices that owns no string. A
//! row entering the top-k therefore costs no allocation, which is the whole point.

/// A row that survived the scalar mask and got an approximate distance. `Copy`, owns nothing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Candidate {
    pub dist: f32,
    pub part: u32,
    pub row: u32,
}

/// A bounded selection keeping the `cap` nearest candidates, in the query contract's order:
/// **`dist` ascending, ties broken on `event_id` ascending** ([D-033](../../../docs/DECISIONS.md),
/// charter C-4).
///
/// It is a *max*-heap on "worse" (larger distance, then larger id), so the root is the worst kept
/// candidate — the one evicted when a nearer row arrives. Because the selection is bounded, this
/// comparator does not merely order the answer; it decides which tied rows are *allowed to be*
/// answers at all, which is why it must be a function of the data and never of the layout.
pub struct TopK<'a> {
    cap: usize,
    heap: Vec<Candidate>,
    id_of: &'a dyn Fn(u32, u32) -> &'a str,
}

impl<'a> TopK<'a> {
    /// `id_of(part, row)` returns the event id of a row, borrowed from an already-resident scalar
    /// column. No allocation happens on the strength of it.
    pub fn new(cap: usize, id_of: &'a dyn Fn(u32, u32) -> &'a str) -> Self {
        TopK {
            cap,
            // One allocation, here, before the scan. The scan itself never grows this.
            heap: Vec::with_capacity(cap.max(1)),
            id_of,
        }
    }

    /// Is `a` the *worse* candidate — nearer the root, evicted first? Larger distance is worse;
    /// on an exact distance tie the larger `event_id` is worse, because the answer keeps the
    /// smallest ids.
    #[inline]
    fn worse(&self, a: Candidate, b: Candidate) -> std::cmp::Ordering {
        a.dist
            .total_cmp(&b.dist)
            .then_with(|| (self.id_of)(a.part, a.row).cmp((self.id_of)(b.part, b.row)))
    }

    #[inline]
    pub fn offer(&mut self, cand: Candidate) {
        if self.heap.len() < self.cap {
            self.heap.push(cand);
            self.sift_up(self.heap.len() - 1);
        } else if let Some(&worst) = self.heap.first() {
            // Keep it only if it is strictly nearer than the worst we currently hold.
            if self.worse(cand, worst) == std::cmp::Ordering::Less {
                self.heap[0] = cand;
                self.sift_down(0);
            }
        }
    }

    fn sift_up(&mut self, mut i: usize) {
        while i > 0 {
            let parent = (i - 1) / 2;
            if self.worse(self.heap[i], self.heap[parent]) == std::cmp::Ordering::Greater {
                self.heap.swap(i, parent);
                i = parent;
            } else {
                break;
            }
        }
    }

    fn sift_down(&mut self, mut i: usize) {
        let n = self.heap.len();
        loop {
            let (l, r) = (2 * i + 1, 2 * i + 2);
            let mut worst = i;
            if l < n && self.worse(self.heap[l], self.heap[worst]) == std::cmp::Ordering::Greater {
                worst = l;
            }
            if r < n && self.worse(self.heap[r], self.heap[worst]) == std::cmp::Ordering::Greater {
                worst = r;
            }
            if worst == i {
                break;
            }
            self.heap.swap(i, worst);
            i = worst;
        }
    }

    pub fn len(&self) -> usize {
        self.heap.len()
    }

    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }

    /// The kept candidates, **nearest first** — `dist` ascending, ties on `event_id` ascending.
    pub fn into_sorted(self) -> Vec<Candidate> {
        let id_of = self.id_of;
        let mut v = self.heap;
        v.sort_by(|&a, &b| {
            a.dist
                .total_cmp(&b.dist)
                .then_with(|| id_of(a.part, a.row).cmp(id_of(b.part, b.row)))
        });
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_nearest_and_breaks_ties_on_id() {
        let ids = ["e5", "e4", "e3", "e2", "e1"];
        let id_of = |_p: u32, row: u32| -> &str { ids[row as usize] };
        let mut t = TopK::new(3, &id_of);
        // All the same distance: selection is decided entirely by the id tie-break.
        for row in 0..5u32 {
            t.offer(Candidate {
                dist: 1.0,
                part: 0,
                row,
            });
        }
        let kept: Vec<&str> = t
            .into_sorted()
            .iter()
            .map(|c| ids[c.row as usize])
            .collect();
        // The three smallest ids, in ascending id order.
        assert_eq!(kept, ["e1", "e2", "e3"]);
    }
}
