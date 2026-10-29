use crate::tax;

pub struct Item {
    pub cents: u64,
}

pub fn total(items: &[Item]) -> u64 {
    tax::add_tax(subtotal(items))
}

fn subtotal(items: &[Item]) -> u64 {
    items.iter().map(|item| item.cents).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn totals_include_tax() {
        assert_eq!(total(&[Item { cents: 100 }]), 123);
    }
}
