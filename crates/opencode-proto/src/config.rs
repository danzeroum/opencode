//! `Config` wire types (`config.get` / `global.config.get`) — `packages/core/src/config`.
//!
//! `Config` is the contract's most complex type (maps + inline unions for `mcp`/`formatter`/`lsp`/
//! `autoupdate`/`plugin`/`references`), so it's ported as a sub-track: this module starts with the
//! self-contained **leaf** config types; the medium types (`AgentConfig`/`ProviderConfig`/`Mcp*`/
//! `Permission*Config`) and the top-level `Config` struct land in follow-up slices (see PENDENCIAS #1).
//! Contract-neutral until `config.get` references them.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Log verbosity (`config.logLevel`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
pub enum LogLevel {
    /// Debug.
    Debug,
    /// Info.
    Info,
    /// Warn.
    Warn,
    /// Error.
    Error,
}

/// Layout mode (`config.layout`, deprecated — always `stretch`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LayoutConfig {
    /// Auto.
    Auto,
    /// Stretch.
    Stretch,
}

/// A policy decision (`allow` | `deny`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PolicyEffect {
    /// Allow.
    Allow,
    /// Deny.
    Deny,
}

/// Server configuration (`config.server`) — for `opencode serve` / the web command.
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct ServerConfig {
    /// Bind port.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<i64>,
    /// Bind hostname.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    /// Whether mDNS advertisement is enabled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mdns: Option<bool>,
    /// mDNS domain.
    #[serde(rename = "mdnsDomain", skip_serializing_if = "Option::is_none")]
    pub mdns_domain: Option<String>,
    /// Allowed CORS origins.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cors: Option<Vec<String>>,
}

/// Image-attachment limits (`config.attachment.image`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct ImageAttachmentConfig {
    /// Auto-resize large images.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_resize: Option<bool>,
    /// Max width (px).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_width: Option<i64>,
    /// Max height (px).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_height: Option<i64>,
    /// Max base64 size (bytes).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_base64_bytes: Option<i64>,
}

/// Attachment configuration (`config.attachment`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct AttachmentConfig {
    /// Image-attachment limits.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<ImageAttachmentConfig>,
}

/// A git-backed reference source (`config.references[*]`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct ConfigV2ReferenceGit {
    /// Repository URL.
    pub repository: String,
    /// Branch, if pinned.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Hidden from the picker.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hidden: Option<bool>,
}

/// A local-path reference source (`config.references[*]`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct ConfigV2ReferenceLocal {
    /// Local path.
    pub path: String,
    /// Description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Hidden from the picker.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hidden: Option<bool>,
}

/// An experimental policy rule (`config.experimental.policies[*]`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct ConfigV2ExperimentalPolicy {
    /// The governed action (currently only `"provider.use"`).
    pub action: String,
    /// Allow or deny.
    pub effect: PolicyEffect,
    /// Resource glob.
    pub resource: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enums_serialize_with_contract_casing() {
        assert_eq!(serde_json::to_value(LogLevel::Warn).unwrap(), "WARN");
        assert_eq!(
            serde_json::to_value(LayoutConfig::Stretch).unwrap(),
            "stretch"
        );
        assert_eq!(serde_json::to_value(PolicyEffect::Deny).unwrap(), "deny");
        let back: LogLevel = serde_json::from_value(serde_json::json!("DEBUG")).unwrap();
        assert_eq!(back, LogLevel::Debug);
    }

    #[test]
    fn server_config_omits_absent_fields_and_renames_mdns_domain() {
        let v = serde_json::to_value(ServerConfig {
            port: Some(4096),
            mdns_domain: Some("local".into()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(v["port"], 4096);
        assert_eq!(v["mdnsDomain"], "local");
        assert!(v.get("hostname").is_none());
        assert!(v.get("cors").is_none());
    }

    #[test]
    fn references_and_policy_round_trip() {
        let git = ConfigV2ReferenceGit {
            repository: "https://x/y".into(),
            branch: None,
            description: None,
            hidden: None,
        };
        let back: ConfigV2ReferenceGit =
            serde_json::from_value(serde_json::to_value(&git).unwrap()).unwrap();
        assert_eq!(back, git);

        let policy = ConfigV2ExperimentalPolicy {
            action: "provider.use".into(),
            effect: PolicyEffect::Allow,
            resource: "anthropic/*".into(),
        };
        let v = serde_json::to_value(&policy).unwrap();
        assert_eq!(v["effect"], "allow");
        assert_eq!(v["action"], "provider.use");
    }
}
