//! Weighted PageRank over files, from the references of the code itself (not of tests, tooling or copies).

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
    /// Per file: a generated copy of another file (see [`Graph::copied_files`]).
    pub copies: Vec<bool>,
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
                copies: Vec::new(),
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
        // what tests, the tooling around the code (`scripts/`) and generated copies use does not make it central:
        // a test of its own would hand a small helper all of its rank. Under the focus everything counts: a map of
        // `scripts/` is a map of what the scripts use
        let copies = self.copied_files();
        let focused = |path: &str| focus.is_some_and(|prefix| !prefix.is_empty() && path.starts_with(prefix));
        let counts = |file: usize| {
            self.files.get(file).is_some_and(|f| {
                focused(&f.path)
                    || (!f.test && !super::is_tooling_path(&f.path) && !copies.get(file).copied().unwrap_or(false))
            })
        };

        for (ri, targets) in self.res.iter().enumerate() {
            let Some(src) = self.refs.get(ri).map(|r| r.file).filter(|src| counts(*src)) else {
                continue;
            };

            // a name matched by its name alone, or a constant read (a table, a config object), says less about which
            // file the code is built around than a call does
            let soft =
                if self.soft.get(ri).copied().unwrap_or(false) || self.refs.get(ri).is_some_and(|r| r.kind == "use") {
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

        Ok(Ranks {
            files: rank,
            syms,
            copies,
        })
    }
}
