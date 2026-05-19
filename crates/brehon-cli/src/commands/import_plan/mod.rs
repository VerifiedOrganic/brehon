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
mod tests;
