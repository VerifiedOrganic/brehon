use clap::ValueEnum;

mod dispatch;
mod extraction;
mod parsing;
mod types;

pub use dispatch::{execute, execute_extract};

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
pub enum ExtractMode {
    Auto,
    Direct,
    Supervisor,
}

#[cfg(test)]
#[path = "tests.rs"]
// Tests use a `std::sync::Mutex` env-serialization guard held across `.await`
// to keep parallel tests from racing on env vars.
#[allow(clippy::await_holding_lock)]
mod tests;
