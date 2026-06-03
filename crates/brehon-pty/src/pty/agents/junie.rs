use std::path::PathBuf;

use crate::pty::config::{PtyConfig, TeamsSpawnConfig};

impl PtyConfig {
    /// Create config for a Junie CLI instance.
    ///
    /// Delegates to [`brehon_adapter_junie::JunieSpawnParams`] for argument
    /// and environment construction, then converts the result into a
    /// [`PtyConfig`].
    #[allow(clippy::too_many_arguments)]
    pub fn junie(
        name: &str,
        role: &str,
        cwd: PathBuf,
        brehon_root: Option<&PathBuf>,
        supervisor_name: Option<&str>,
        factory_worker_cli: Option<&str>,
        model: Option<&str>,
        _teams: Option<&TeamsSpawnConfig>,
    ) -> Self {
        let params = brehon_adapter_junie::JunieSpawnParams {
            name: name.to_string(),
            role: role.to_string(),
            cwd,
            brehon_root: brehon_root.cloned(),
            supervisor_name: supervisor_name.map(|s| s.to_string()),
            factory_worker_cli: factory_worker_cli.map(|s| s.to_string()),
            model: model.map(|m| m.to_string()),
        };

        let config = brehon_adapter_junie::JunieSessionConfig::from_params(&params);

        Self {
            command: config.command,
            args: config.args,
            cwd: config.cwd,
            env: config.env,
            rows: config.rows,
            cols: config.cols,
        }
    }
}
