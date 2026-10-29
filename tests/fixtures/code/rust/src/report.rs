/// A different `total`: the sum of a plain list of numbers, unrelated to pricing.
pub fn total(values: &[u64]) -> u64 {
    values.iter().sum()
}

pub fn summary(values: &[u64]) -> String {
    format!("{} entries, {} in total", values.len(), total(values))
}

/// Same method name as `Order::cost`, a different type: only a receiver type tells them apart.
pub struct Quote {
    pub cents: u64,
}

impl Quote {
    pub fn cost(&self) -> u64 {
        self.cents
    }
}
