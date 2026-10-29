pub const RATE_PERCENT: u64 = 23;

pub fn add_tax(net: u64) -> u64 {
    net + net * RATE_PERCENT / 100
}
