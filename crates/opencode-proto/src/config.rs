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

/// A permission decision in config (`ask` | `allow` | `deny`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PermissionActionConfig {
    /// Ask the user.
    Ask,
    /// Allow.
    Allow,
    /// Deny.
    Deny,
}

/// A per-pattern permission map (`{ "<glob>": action }`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(transparent)]
pub struct PermissionObjectConfig(pub std::collections::BTreeMap<String, PermissionActionConfig>);

/// A permission rule in config: a flat action, or a per-pattern map
/// (`anyOf[PermissionActionConfig, PermissionObjectConfig]`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(untagged)]
pub enum PermissionRuleConfig {
    /// A single action for the whole permission.
    Action(PermissionActionConfig),
    /// Per-pattern actions.
    Object(PermissionObjectConfig),
}

/// Per-tool permission overrides (`config.permission` detailed form). Each field is a rule (most) or a
/// flat action (the no-pattern tools); unknown tools fall through `additionalProperties` (dropped by the
/// contract normalizer, so not modeled).
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct PermissionDetailedConfig {
    /// `read` tool.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read: Option<PermissionRuleConfig>,
    /// `edit` tool.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edit: Option<PermissionRuleConfig>,
    /// `glob` tool.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub glob: Option<PermissionRuleConfig>,
    /// `grep` tool.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grep: Option<PermissionRuleConfig>,
    /// `list` tool.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub list: Option<PermissionRuleConfig>,
    /// `bash` tool.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bash: Option<PermissionRuleConfig>,
    /// `task` tool.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<PermissionRuleConfig>,
    /// `external_directory` access.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_directory: Option<PermissionRuleConfig>,
    /// `todowrite` tool.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub todowrite: Option<PermissionActionConfig>,
    /// `question` tool.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub question: Option<PermissionActionConfig>,
    /// `webfetch` tool.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub webfetch: Option<PermissionActionConfig>,
    /// `websearch` tool.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub websearch: Option<PermissionActionConfig>,
    /// `lsp` tool.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lsp: Option<PermissionRuleConfig>,
    /// `doom_loop` guard.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doom_loop: Option<PermissionActionConfig>,
    /// `skill` tool.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skill: Option<PermissionRuleConfig>,
}

/// `config.permission` / `AgentConfig.permission`: a flat action applied to everything, or per-tool
/// overrides (`anyOf[PermissionActionConfig, PermissionDetailedConfig]`).
// The `Detailed` variant is inherently far larger than the flat `Action`; this is a short-lived wire
// DTO, so boxing to satisfy the stack-size lint would only add indirection.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(untagged)]
pub enum PermissionConfig {
    /// One action for all tools.
    Action(PermissionActionConfig),
    /// Per-tool overrides.
    Detailed(PermissionDetailedConfig),
}

/// OAuth configuration for a remote MCP server (`config.mcp[*].oauth`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct McpOAuthConfig {
    /// OAuth client id.
    #[serde(rename = "clientId", skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// OAuth client secret.
    #[serde(rename = "clientSecret", skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    /// Requested scope.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Local callback port.
    #[serde(rename = "callbackPort", skip_serializing_if = "Option::is_none")]
    pub callback_port: Option<i64>,
    /// Redirect URI.
    #[serde(rename = "redirectUri", skip_serializing_if = "Option::is_none")]
    pub redirect_uri: Option<String>,
}

/// A remote MCP server's `oauth` setting: a config block, or `false` to disable OAuth auto-detection
/// (`anyOf[McpOAuthConfig, false]`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(untagged)]
pub enum McpOAuthSetting {
    /// Explicit OAuth config.
    Config(McpOAuthConfig),
    /// `false` — disable OAuth auto-detection.
    Disabled(bool),
}

/// A local (stdio) MCP server (`config.mcp[*]`, `type: "local"`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct McpLocalConfig {
    /// Always `"local"`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Command + args to launch the server.
    pub command: Vec<String>,
    /// Working directory.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Environment variables (map).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub environment: Option<serde_json::Value>,
    /// Whether the server is enabled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Connection timeout (ms).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<i64>,
}

/// A remote (HTTP/SSE) MCP server (`config.mcp[*]`, `type: "remote"`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct McpRemoteConfig {
    /// Always `"remote"`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Server URL.
    pub url: String,
    /// Whether the server is enabled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Extra request headers (map).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub headers: Option<serde_json::Value>,
    /// OAuth setting.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oauth: Option<McpOAuthSetting>,
    /// Connection timeout (ms).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<i64>,
}

/// An agent definition in config (`config.agent[*]` / `config.mode[*]`). All fields optional; unknown
/// keys fall through `additionalProperties` (dropped by the normalizer). `tools`/`options` are maps →
/// `Value`; `color` is a hex-or-theme string (both arms normalize to `{type:string}`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct AgentConfig {
    /// Default model (`provider/model`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Experimental-mode variant.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    /// Sampling temperature.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Nucleus-sampling top-p.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    /// System prompt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// Per-tool enable/disable map.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub tools: Option<serde_json::Value>,
    /// Whether the agent is disabled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disable: Option<bool>,
    /// Description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// `subagent` | `primary` | `all`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// Hidden from the picker.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hidden: Option<bool>,
    /// Provider/AI-SDK options (free-form).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub options: Option<serde_json::Value>,
    /// Hex (`#RRGGBB`) or theme color (`primary`/`accent`/…).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// Max steps per turn.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub steps: Option<i64>,
    /// Max steps per turn (alias).
    #[serde(rename = "maxSteps", skip_serializing_if = "Option::is_none")]
    pub max_steps: Option<i64>,
    /// Permission overrides for this agent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission: Option<PermissionConfig>,
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

    #[test]
    fn permission_config_flat_and_detailed_round_trip() {
        // Flat form: a single action string.
        let flat: PermissionConfig = serde_json::from_value(serde_json::json!("ask")).unwrap();
        assert_eq!(flat, PermissionConfig::Action(PermissionActionConfig::Ask));
        assert_eq!(serde_json::to_value(&flat).unwrap(), "ask");

        // Detailed form: per-tool overrides; `bash` as a per-pattern map, `webfetch` as a flat action.
        let detailed: PermissionConfig = serde_json::from_value(serde_json::json!({
            "bash": { "git *": "allow", "rm *": "deny" },
            "edit": "ask",
            "webfetch": "deny"
        }))
        .unwrap();
        match &detailed {
            PermissionConfig::Detailed(d) => {
                assert!(matches!(d.bash, Some(PermissionRuleConfig::Object(_))));
                assert!(matches!(
                    d.edit,
                    Some(PermissionRuleConfig::Action(PermissionActionConfig::Ask))
                ));
                assert_eq!(d.webfetch, Some(PermissionActionConfig::Deny));
            }
            other => panic!("expected detailed, got {other:?}"),
        }
        // Round-trips back to the same JSON shape.
        let back: PermissionConfig =
            serde_json::from_value(serde_json::to_value(&detailed).unwrap()).unwrap();
        assert_eq!(back, detailed);
    }

    #[test]
    fn mcp_local_and_remote_configs_round_trip() {
        let local: McpLocalConfig = serde_json::from_value(serde_json::json!({
            "type": "local",
            "command": ["bun", "x", "server"],
            "environment": { "TOKEN": "x" }
        }))
        .unwrap();
        assert_eq!(local.kind, "local");
        assert_eq!(local.command, ["bun", "x", "server"]);
        assert_eq!(local.environment.as_ref().unwrap()["TOKEN"], "x");

        // `oauth: false` decodes to the Disabled arm; an object decodes to Config.
        let remote_off: McpRemoteConfig = serde_json::from_value(serde_json::json!({
            "type": "remote", "url": "https://x", "oauth": false
        }))
        .unwrap();
        assert!(matches!(
            remote_off.oauth,
            Some(McpOAuthSetting::Disabled(false))
        ));
        let remote_oauth: McpRemoteConfig = serde_json::from_value(serde_json::json!({
            "type": "remote", "url": "https://x", "oauth": { "clientId": "abc" }
        }))
        .unwrap();
        match remote_oauth.oauth {
            Some(McpOAuthSetting::Config(c)) => assert_eq!(c.client_id.as_deref(), Some("abc")),
            other => panic!("expected oauth config, got {other:?}"),
        }
    }

    #[test]
    fn agent_config_round_trips_with_maps_and_permission() {
        let agent: AgentConfig = serde_json::from_value(serde_json::json!({
            "model": "anthropic/claude",
            "mode": "primary",
            "temperature": 0.3,
            "tools": { "bash": false },
            "color": "#ff5733",
            "maxSteps": 12,
            "permission": { "bash": "ask" }
        }))
        .unwrap();
        assert_eq!(agent.model.as_deref(), Some("anthropic/claude"));
        assert_eq!(agent.mode.as_deref(), Some("primary"));
        assert_eq!(agent.tools.as_ref().unwrap()["bash"], false);
        assert_eq!(agent.color.as_deref(), Some("#ff5733"));
        assert_eq!(agent.max_steps, Some(12));
        assert!(matches!(
            agent.permission,
            Some(PermissionConfig::Detailed(_))
        ));
        let back: AgentConfig =
            serde_json::from_value(serde_json::to_value(&agent).unwrap()).unwrap();
        assert_eq!(back, agent);
    }
}
