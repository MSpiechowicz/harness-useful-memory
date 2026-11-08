//! One case's surroundings: a database of its own, a proxy in front of it, and the repository
//! its users share.

use super::live::{Kind, Live};
use super::proxy::Proxy;
use super::user::{User, origin};

pub struct World {
    pub live: Live,
    /// The network of every user made with [`Self::user`].
    pub proxy: Proxy,
    origin: String,
}

impl World {
    /// The surroundings of `case` on a database of `kind`, or `None` (with a message) when no
    /// server of that kind is configured.
    pub fn open(kind: Kind, case: &str) -> Option<Self> {
        let live = Live::open(kind)?;
        let proxy = Proxy::to(&live.authority());

        Some(Self {
            origin: origin(&format!("{case}-{kind}-{}", std::process::id())),
            live,
            proxy,
        })
    }

    /// A user whose connection runs through the proxy: the one the outages happen to.
    pub fn user(&self, name: &str) -> User {
        User::new(name, &self.origin, &self.live, Some(&self.proxy.authority()))
    }

    /// A teammate who is always online, connected to the database directly.
    pub fn teammate(&self, name: &str) -> User {
        User::new(name, &self.origin, &self.live, None)
    }
}
