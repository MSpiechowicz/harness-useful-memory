//! Weighted PageRank over files.

use std::collections::HashMap;

use super::Graph;

/// Teleport and walk: the usual damping.
const DAMPING: f64 = 0.85;

const ITERATIONS: usize = 30;

/// What a method call matched by its name alone counts, against a call whose receiver type is known: a
/// name like `add` or `get` matches every class that has it, so it says little about who depends on whom.
const SOFT_WEIGHT: f64 = 0.2;

/// File and symbol scores.
pub(super) struct Ranks {
    /// Per file.
    pub files: Vec<f64>,
    /// Per symbol: the rank that flows into it from other files.
    pub syms: Vec<f64>,
}

impl Graph {
    /// PageRank of the files (optionally personalised to a path prefix) and the share of it each symbol
    /// receives from other files.
    pub(super) fn rank(&self, focus: Option<&str>) -> Ranks {
        match self.rank_paced(focus, &|| Ok(())) {
            Ok(ranks) => ranks,
            // `tick` of no check never fails
            Err(_) => Ranks {
                files: Vec::new(),
                syms: Vec::new(),
            },
        }
    }

    /// [`Graph::rank`], asking `tick` between the steps (the iterations are the long part): a call whose time
    /// is spent stops there instead of running on.
    pub(super) fn rank_paced(
        &self,
        focus: Option<&str>,
        tick: &dyn Fn() -> Result<(), String>,
    ) -> Result<Ranks, String> {
        let n = self.files.len();
        let mut out_weight = vec![0.0_f64; n];
        let mut edges: Vec<(usize, usize, usize, f64)> = Vec::new(); // (from file, to file, symbol, weight)

        for (ri, targets) in self.res.iter().enumerate() {
            let Some(src) = self.refs.get(ri).map(|r| r.file) else {
                continue;
            };

            let soft = if self.soft.get(ri).copied().unwrap_or(false) {
                SOFT_WEIGHT
            } else {
                1.0
            };

            for (sym, weight) in targets {
                let Some(dst) = self.syms.get(*sym).map(|s| s.file) else {
                    continue;
                };

                if dst != src {
                    let weight = f64::from(*weight) * soft;

                    edges.push((src, dst, *sym, weight));

                    if let Some(total) = out_weight.get_mut(src) {
                        *total += weight;
                    }
                }
            }
        }

        // many mentions between the same two files count less than their number: the square root of
        // their sum, shared out over the mentions
        let mut pair_sum: HashMap<(usize, usize), f64> = HashMap::new();

        for (src, dst, _, weight) in &edges {
            *pair_sum.entry((*src, *dst)).or_default() += weight;
        }

        out_weight.iter_mut().for_each(|total| *total = 0.0);

        for (src, dst, _, weight) in &mut edges {
            let sum = pair_sum.get(&(*src, *dst)).copied().unwrap_or(1.0);

            *weight *= sum.sqrt() / sum;

            if let Some(total) = out_weight.get_mut(*src) {
                *total += *weight;
            }
        }

        let teleport: Vec<f64> = match focus.filter(|prefix| !prefix.is_empty()) {
            Some(prefix) => {
                let hits = self.files.iter().filter(|f| f.path.starts_with(prefix)).count().max(1) as f64;

                self.files
                    .iter()
                    .map(|f| if f.path.starts_with(prefix) { 1.0 / hits } else { 0.0 })
                    .collect()
            }
            None => vec![1.0 / n.max(1) as f64; n],
        };

        let mut rank = teleport.clone();

        for _ in 0..ITERATIONS {
            tick()?;

            let dangling: f64 = (0..n)
                .filter(|i| out_weight.get(*i).copied().unwrap_or(0.0) == 0.0)
                .map(|i| rank.get(i).copied().unwrap_or(0.0))
                .sum();
            let mut next: Vec<f64> = teleport
                .iter()
                .map(|t| (1.0 - DAMPING) * t + DAMPING * dangling * t)
                .collect();

            for (src, dst, _, weight) in &edges {
                let total = out_weight.get(*src).copied().unwrap_or(0.0);

                if total > 0.0
                    && let Some(slot) = next.get_mut(*dst)
                {
                    *slot += DAMPING * rank.get(*src).copied().unwrap_or(0.0) * weight / total;
                }
            }

            rank = next;
        }

        let mut syms = vec![0.0_f64; self.syms.len()];

        for (src, _, sym, weight) in &edges {
            let total = out_weight.get(*src).copied().unwrap_or(0.0);

            if total > 0.0
                && let Some(slot) = syms.get_mut(*sym)
            {
                *slot += rank.get(*src).copied().unwrap_or(0.0) * weight / total;
            }
        }

        tick()?;

        Ok(Ranks { files: rank, syms })
    }
}
