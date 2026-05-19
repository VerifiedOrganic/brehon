//! Kimi Code CLI adapter for Brehon.
//!
//! This crate implements the [`AgentAdapter`] trait for the Kimi CLI,
//! enabling Brehon to manage Kimi sessions through a structured adapter
//! interface. It also hosts the Kimi-specific runtime configuration helpers
//! previously in `brehon-pty`.

pub mod acp_types;
pub mod kimi;

pub use kimi::{
    build_kimi_spawn_config, desired_kimi_mcp_config, kimi_share_dir, prepare_local_kimi_runtime,
    prepare_local_kimi_runtime_with_global_share, KimiAdapter, KimiConfig, KimiError, KimiSession,
    KimiSessionInner, KimiSpawnConfig,
};
