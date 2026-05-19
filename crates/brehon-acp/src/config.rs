//! Session config application.
//!
//! Handles applying config options to sessions and checking support for options.

use std::collections::HashSet;

use tracing::warn;

use brehon_types::AgentCapabilities;

#[derive(Debug, Clone)]
pub struct ConfigOption {
    pub name: String,
    #[allow(dead_code)]
    pub value: String,
    pub required: bool,
}

pub struct ConfigManager {
    supported_options: HashSet<String>,
}

impl ConfigManager {
    pub fn new(capabilities: &AgentCapabilities) -> Self {
        Self {
            supported_options: capabilities
                .session_config_options
                .iter()
                .cloned()
                .collect(),
        }
    }

    pub fn can_apply(&self, option: &str) -> bool {
        self.supported_options.contains(option)
    }

    pub fn check_support<'a>(
        &self,
        options: &'a [ConfigOption],
    ) -> Result<Vec<&'a ConfigOption>, ConfigError> {
        let mut unsupported_required = Vec::new();

        for opt in options {
            if opt.required && !self.can_apply(&opt.name) {
                warn!(option = %opt.name, required = opt.required, "Unsupported required config option");
                unsupported_required.push(opt);
            }
        }

        if !unsupported_required.is_empty() {
            let names: Vec<&str> = unsupported_required
                .iter()
                .map(|o| o.name.as_str())
                .collect();
            return Err(ConfigError::UnsupportedRequired(format!(
                "Required config options not supported: {}",
                names.join(", ")
            )));
        }

        let applicable: Vec<&'a ConfigOption> = options
            .iter()
            .filter(|opt| self.can_apply(&opt.name) || opt.required)
            .collect();

        Ok(applicable)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("Unsupported required option: {0}")]
    UnsupportedRequired(String),
    #[error("Config application failed: {0}")]
    #[allow(dead_code)]
    ApplicationFailed(String),
    #[error("Invalid config value: {0}")]
    #[allow(dead_code)]
    InvalidValue(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use brehon_types::ToolCallStreaming;

    fn test_capabilities() -> AgentCapabilities {
        AgentCapabilities {
            content_block_types: vec!["text".to_string()],
            session_config_options: vec!["model".to_string(), "timeout".to_string()],
            permission_support: true,
            terminal_support: false,
            tool_call_streaming: ToolCallStreaming::Basic,
        }
    }

    fn make_option(name: &str, value: &str, required: bool) -> ConfigOption {
        ConfigOption {
            name: name.to_string(),
            value: value.to_string(),
            required,
        }
    }

    #[test]
    fn test_can_apply() {
        let caps = test_capabilities();
        let manager = ConfigManager::new(&caps);

        assert!(manager.can_apply("model"));
        assert!(manager.can_apply("timeout"));
        assert!(!manager.can_apply("unknown"));
    }

    #[test]
    fn test_check_support_all_supported() {
        let caps = test_capabilities();
        let manager = ConfigManager::new(&caps);

        let options = vec![
            make_option("model", "gpt-4", false),
            make_option("timeout", "60", false),
        ];

        let result = manager.check_support(&options);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().len(), 2);
    }

    #[test]
    fn test_check_support_unsupported_optional() {
        let caps = test_capabilities();
        let manager = ConfigManager::new(&caps);

        let options = vec![
            make_option("model", "gpt-4", false),
            make_option("unknown", "value", false),
        ];

        let result = manager.check_support(&options);
        assert!(result.is_ok());
    }

    #[test]
    fn test_check_support_unsupported_required() {
        let caps = test_capabilities();
        let manager = ConfigManager::new(&caps);

        let options = vec![
            make_option("model", "gpt-4", false),
            make_option("unknown", "value", true),
        ];

        let result = manager.check_support(&options);
        assert!(result.is_err());
    }
}
