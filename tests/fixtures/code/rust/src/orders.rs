use crate::pricing::{self, Item};

pub struct Order {
    items: Vec<Item>,
}

impl Order {
    pub fn new(items: Vec<Item>) -> Self {
        Order { items }
    }

    pub fn cost(&self) -> u64 {
        pricing::total(&self.items)
    }
}

pub fn checkout(order: &Order) -> u64 {
    order.cost()
}
