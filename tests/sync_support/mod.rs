//! The pieces of the sync suites (`sync_*.rs`): a TCP proxy whose network can be cut, the live
//! databases, simulated users running the real executable, and what they and a second user read.
//!
//! The suites need `mod common; mod libsql_support; mod mongo_support; mod postgres_support;` next
//! to `mod sync_support;`, and run on every backend whose test variable is set (see
//! `HARNESS_MEMORY_TEST_POSTGRES_URL`, `HARNESS_MEMORY_TEST_LIBSQL_URL`,
//! `HARNESS_MEMORY_TEST_MONGO_URL`): without one a case prints that it is skipped and passes.
#![allow(dead_code)]

pub mod live;
pub mod mcp;
pub mod proxy;
pub mod reads;
pub mod user;
pub mod world;

#[allow(unused_imports)]
pub use live::{Kind, Live};
#[allow(unused_imports)]
pub use mcp::{Agent, Reply};
#[allow(unused_imports)]
pub use proxy::Proxy;
#[allow(unused_imports)]
pub use reads::{
    Place, Reading, held_back, local_counts, local_notes, local_script_names, local_scripts, local_titles,
    pass_a_second, waiting,
};
#[allow(unused_imports)]
pub use user::{Places, REMOTE, REMOTE_LOCAL, REMOTE_LOCAL_DEFAULT, User, origin};
#[allow(unused_imports)]
pub use world::World;

/// `sync_cases!(name)` runs `fn name(kind: Kind)` as `name::postgres`, `name::libsql`,
/// `name::mongodb` and `name::documentdb`.
#[macro_export]
macro_rules! sync_cases {
    ($($name:ident),* $(,)?) => {
        $(
            mod $name {
                #[test]
                fn postgres() {
                    super::$name($crate::sync_support::Kind::Postgres);
                }

                #[test]
                fn libsql() {
                    super::$name($crate::sync_support::Kind::Libsql);
                }

                #[test]
                fn mongodb() {
                    super::$name($crate::sync_support::Kind::Mongodb);
                }

                #[test]
                fn documentdb() {
                    super::$name($crate::sync_support::Kind::Documentdb);
                }
            }
        )*
    };
}
