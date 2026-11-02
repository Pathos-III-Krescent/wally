use anyhow::ensure;
use semver::Version;
use serde::{Deserialize, Deserializer, Serialize};
use url::Url;

use crate::{auth::AuthMode, storage::StorageMode};

#[derive(Deserialize, Serialize)]
pub struct Config {
    /// The URL of the Git repository containing the registry's package index.
    pub index_url: Url,

    /// The token that should be used by the registry to communicate with
    /// GitHub. If not specified, will try to use the machine's Git credential
    /// helper.
    #[serde(default, deserialize_with = "empty_as_none")]
    pub github_token: Option<String>,

    /// What kind of authentication is required to access endpoints.
    pub auth: AuthMode,

    /// Which storage backend to use.
    pub storage: StorageMode,

    /// The minimum wally cli version required to publish to the registry
    pub minimum_wally_version: Option<Version>,

    /// In `github-org` mode, a key that can publish (but not read) packages.
    /// Intended for CI, which can't do GitHub's device flow.
    #[serde(default, deserialize_with = "empty_as_none")]
    pub ci_publish_key: Option<String>,

    /// In `github-org` mode, an old shared API key that can still read and
    /// publish. Only meant for migrating off `api-key` mode; remove it once
    /// everyone has run `wally login`.
    #[serde(default, deserialize_with = "empty_as_none")]
    pub legacy_api_key: Option<String>,

    /// File that audit log lines are appended to, in addition to stdout.
    #[serde(default, deserialize_with = "empty_as_none")]
    pub audit_log_path: Option<String>,

    /// Discord webhook URL that publishes and denied requests are sent to.
    #[serde(default, deserialize_with = "empty_as_none")]
    pub audit_webhook: Option<String>,

    /// Base URL of the GitHub API. Only needs changing for tests.
    #[serde(default = "default_github_api_url")]
    pub github_api_url: String,
}

impl Config {
    /// Refuses settings that would leave the registry more open than intended.
    pub fn validate(&self) -> anyhow::Result<()> {
        if let AuthMode::GithubOrg { cache_ttl_secs, .. } = &self.auth {
            ensure!(
                *cache_ttl_secs <= 60,
                "cache-ttl-secs must be 60 or less, so removed users lose access within a minute"
            );
            ensure!(
                self.github_token.is_some(),
                "github-org auth needs github_token to check org and team membership"
            );
        } else {
            ensure!(
                self.ci_publish_key.is_none() && self.legacy_api_key.is_none(),
                "ci_publish_key and legacy_api_key only work with github-org auth"
            );
        }

        Ok(())
    }
}

fn default_github_api_url() -> String {
    "https://api.github.com".to_owned()
}

/// Docker Compose turns an unset `${VAR}` into an empty string. Treat that the
/// same as the setting being absent so an empty key can never match anything.
fn empty_as_none<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    let value = Option::<String>::deserialize(deserializer)?;
    Ok(value.filter(|value| !value.trim().is_empty()))
}
