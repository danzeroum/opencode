//! Integration wire types — `v2.integration.list` (`GET /api/integration`).
//!
//! Integrations (GitHub/GitLab/Slack …) are a greenfield Rust concern (Phase 3c). This module models
//! the full `IntegrationInfo` closure so the list route is contract-complete; until the integration
//! runtime lands the route returns an empty list. The method/prompt/connection unions are serde
//! **internally-tagged** on `type` (matching the golden `anyOf`s of named variant schemas).

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::LocationInfo;

/// A conditional gate on a prompt (`IntegrationWhen`): show the prompt only when `key`'s value
/// satisfies `op` against `value`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct IntegrationWhen {
    /// The prompt key to test.
    pub key: String,
    /// Comparison operator (`eq` | `neq`).
    pub op: String,
    /// The value to compare against.
    pub value: String,
}

/// A choice for a select prompt (`IntegrationSelectPrompt.options` item): `{ label, value, hint? }`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct IntegrationSelectOption {
    /// Display label.
    pub label: String,
    /// Submitted value.
    pub value: String,
    /// Optional hint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

/// A prompt shown while connecting an integration (`IntegrationTextPrompt` | `IntegrationSelectPrompt`),
/// internally tagged on `type`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum IntegrationPrompt {
    /// A free-text prompt.
    Text {
        /// The answer key.
        key: String,
        /// The prompt message.
        message: String,
        /// Optional input placeholder.
        #[serde(skip_serializing_if = "Option::is_none")]
        placeholder: Option<String>,
        /// Optional conditional gate.
        #[serde(skip_serializing_if = "Option::is_none")]
        when: Option<IntegrationWhen>,
    },
    /// A single-choice prompt.
    Select {
        /// The answer key.
        key: String,
        /// The prompt message.
        message: String,
        /// The available choices.
        options: Vec<IntegrationSelectOption>,
        /// Optional conditional gate.
        #[serde(skip_serializing_if = "Option::is_none")]
        when: Option<IntegrationWhen>,
    },
}

/// A way to connect an integration (`IntegrationOAuthMethod` | `IntegrationKeyMethod` |
/// `IntegrationEnvMethod`), internally tagged on `type`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum IntegrationMethod {
    /// OAuth connection flow.
    Oauth {
        /// Method id.
        id: String,
        /// Display label.
        label: String,
        /// Prompts to collect before starting the flow.
        #[serde(skip_serializing_if = "Option::is_none")]
        prompts: Option<Vec<IntegrationPrompt>>,
    },
    /// API-key connection.
    Key {
        /// Optional display label.
        #[serde(skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    /// Environment-variable connection.
    Env {
        /// The environment variable names that enable this integration.
        names: Vec<String>,
    },
}

/// An existing connection for an integration (`ConnectionCredentialInfo` | `ConnectionEnvInfo`),
/// internally tagged on `type`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ConnectionInfo {
    /// A stored-credential connection.
    Credential {
        /// Credential id.
        id: String,
        /// Display label.
        label: String,
    },
    /// An environment-variable connection.
    Env {
        /// The environment variable name in effect.
        name: String,
    },
}

/// An integration entry (`v2.integration.list` item). Mirrors the golden `IntegrationInfo`:
/// `{ id, name, methods, connections }`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct IntegrationInfo {
    /// Integration id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// The connection methods this integration offers.
    pub methods: Vec<IntegrationMethod>,
    /// The integration's existing connections.
    pub connections: Vec<ConnectionInfo>,
}

/// 200 body of `v2.integration.list` (GET /api/integration): the `Location.response` wrapper
/// `{ location, data }` around the integration list.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct IntegrationListResponse {
    /// The resolved request location.
    pub location: LocationInfo,
    /// The integrations.
    pub data: Vec<IntegrationInfo>,
}
