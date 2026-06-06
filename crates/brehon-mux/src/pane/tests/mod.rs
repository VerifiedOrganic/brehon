use crate::harness::{AgentAdapter, SupervisorCli};

mod basic;
mod context;
mod gateway;
mod grok_sandbox;
mod injection;
mod supervisor;

pub fn builtin(cli: SupervisorCli) -> AgentAdapter {
    AgentAdapter::BuiltIn(cli)
}
