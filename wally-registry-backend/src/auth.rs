use std::fmt;

use anyhow::format_err;
use constant_time_eq::constant_time_eq;
use libwally::{package_id::PackageId, package_index::PackageIndex};
use reqwest::Client;
use rocket::{
    Request, State,
    http::Status,
    request::{FromRequest, Outcome},
};
use serde::{Deserialize, Serialize};

use crate::audit::{Actor, AuditContext};
use crate::error::Error;
use crate::github::{GithubInfo, GithubOrgAuth, Group, verify_github_token};
use crate::{config::Config, error::ApiErrorStatus};

#[derive(Deserialize, Serialize)]
#[serde(tag = "type", content = "value", rename_all = "kebab-case")]
pub enum AuthMode {
    ApiKey(String),
    DoubleApiKey {
        read: Option<String>,
        write: String,
    },
    GithubOAuth {
        #[serde(rename = "client-id")]
        client_id: String,
        #[serde(rename = "client-secret")]
        client_secret: String,
    },
    /// Reading requires membership of a GitHub org (or one of its teams) and
    /// publishing requires membership of a team.
    #[serde(rename_all = "kebab-case")]
    GithubOrg {
        client_id: String,
        client_secret: String,
        org: String,
        reader_team: Option<String>,
        publisher_team: String,
        #[serde(default = "default_cache_ttl_secs")]
        cache_ttl_secs: u64,
    },
    Unauthenticated,
}

fn default_cache_ttl_secs() -> u64 {
    60
}

impl fmt::Debug for AuthMode {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        match self {
            AuthMode::ApiKey(_) => write!(formatter, "API key"),
            AuthMode::DoubleApiKey { .. } => write!(formatter, "double API key"),
            AuthMode::GithubOAuth { .. } => write!(formatter, "Github OAuth"),
            AuthMode::GithubOrg {
                org,
                reader_team,
                publisher_team,
                ..
            } => write!(
                formatter,
                "Github org (org: {}, reader team: {}, publisher team: {})",
                org,
                reader_team.as_deref().unwrap_or("<whole org>"),
                publisher_team
            ),
            AuthMode::Unauthenticated => write!(formatter, "no authentication"),
        }
    }
}

fn bearer_token<'r>(request: &'r Request<'_>) -> Option<&'r str> {
    let token = request
        .headers()
        .get_one("authorization")?
        .strip_prefix("Bearer ")?
        .trim();

    (!token.is_empty()).then_some(token)
}

fn matches_key(key: Option<&str>, token: &str) -> bool {
    key.is_some_and(|key| constant_time_eq(key.as_bytes(), token.as_bytes()))
}

fn match_api_key<T>(request: &Request<'_>, key: &str, result: T) -> Outcome<T, Error> {
    let input_api_key = match bearer_token(request) {
        Some(token) => token,
        None => {
            return format_err!("API key required")
                .status(Status::Unauthorized)
                .into();
        }
    };

    if constant_time_eq(key.as_bytes(), input_api_key.as_bytes()) {
        AuditContext::of(request).set_actor(Actor::ApiKey);
        Outcome::Success(result)
    } else {
        format_err!("Invalid API key for read access")
            .status(Status::Unauthorized)
            .into()
    }
}

/// Who a request in `github-org` mode authenticated as.
enum OrgIdentity {
    LegacyKey,
    CiKey,
    User(GithubInfo),
}

async fn github_org_identity(
    request: &Request<'_>,
    config: &Config,
    client_id: &str,
    client_secret: &str,
) -> Result<OrgIdentity, Error> {
    let audit = AuditContext::of(request);
    let token = bearer_token(request).ok_or_else(|| {
        format_err!("GitHub login required. Run `wally login` and try again.")
            .status(Status::Unauthorized)
    })?;

    if matches_key(config.legacy_api_key.as_deref(), token) {
        audit.set_actor(Actor::LegacyKey);
        return Ok(OrgIdentity::LegacyKey);
    }

    if matches_key(config.ci_publish_key.as_deref(), token) {
        audit.set_actor(Actor::CiKey);
        return Ok(OrgIdentity::CiKey);
    }

    let github = request
        .rocket()
        .state::<GithubOrgAuth>()
        .expect("GithubOrgAuth was not configured");

    let user = github.user(token, client_id, client_secret).await?;
    audit.set_actor(Actor::Github(user.clone()));

    Ok(OrgIdentity::User(user))
}

async fn require_membership(request: &Request<'_>, login: &str, group: Group) -> Result<(), Error> {
    let github = request
        .rocket()
        .state::<GithubOrgAuth>()
        .expect("GithubOrgAuth was not configured");

    if github.is_member(login, &group).await? {
        return Ok(());
    }

    let group = match group {
        Group::Org => "the GitHub organization".to_owned(),
        Group::Team(team) => format!("the {} GitHub team", team),
    };

    Err(format_err!("{} is not a member of {}", login, group).status(Status::Forbidden))
}

fn into_outcome<T>(result: Result<T, Error>) -> Outcome<T, Error> {
    match result {
        Ok(value) => Outcome::Success(value),
        Err(err) => err.into(),
    }
}

pub enum ReadAccess {
    Public,
    ApiKey,
    Github,
}

impl ReadAccess {
    async fn github_org(
        request: &Request<'_>,
        config: &Config,
        client_id: &str,
        client_secret: &str,
        reader_team: Option<&str>,
    ) -> Result<Self, Error> {
        match github_org_identity(request, config, client_id, client_secret).await? {
            OrgIdentity::LegacyKey => Ok(ReadAccess::ApiKey),
            OrgIdentity::CiKey => {
                Err(format_err!("The CI key can only publish packages").status(Status::Forbidden))
            }
            OrgIdentity::User(user) => {
                let group = match reader_team {
                    Some(team) => Group::Team(team.to_owned()),
                    None => Group::Org,
                };
                require_membership(request, user.login(), group).await?;
                Ok(ReadAccess::Github)
            }
        }
    }
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for ReadAccess {
    type Error = Error;

    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, Error> {
        let config = request
            .guard::<&State<Config>>()
            .await
            .expect("AuthMode was not configured");

        match &config.auth {
            AuthMode::Unauthenticated => Outcome::Success(ReadAccess::Public),
            AuthMode::GithubOAuth { .. } => Outcome::Success(ReadAccess::Public),
            AuthMode::ApiKey(key) => match_api_key(request, key, ReadAccess::ApiKey),
            AuthMode::DoubleApiKey { read, .. } => match read {
                None => Outcome::Success(ReadAccess::Public),
                Some(key) => match_api_key(request, key, ReadAccess::ApiKey),
            },
            AuthMode::GithubOrg {
                client_id,
                client_secret,
                reader_team,
                ..
            } => into_outcome(
                ReadAccess::github_org(
                    request,
                    config,
                    client_id,
                    client_secret,
                    reader_team.as_deref(),
                )
                .await,
            ),
        }
    }
}

pub enum WriteAccess {
    ApiKey,
    Github(GithubInfo),
    /// A member of the publisher team in `github-org` mode, who may publish
    /// to any scope.
    Publisher,
}

impl WriteAccess {
    pub fn can_write_package(
        &self,
        package_id: &PackageId,
        index: &PackageIndex,
    ) -> anyhow::Result<bool> {
        let scope = package_id.name().scope();

        let has_permission = match self {
            WriteAccess::ApiKey | WriteAccess::Publisher => true,
            WriteAccess::Github(github_info) => {
                match index.is_scope_owner(scope, github_info.id())? {
                    true => true,
                    // Only grant write access if the username matches the scope AND the scope has no existing owners
                    false => {
                        github_info.login().to_lowercase() == scope
                            && index.get_scope_owners(scope)?.is_empty()
                    }
                }
            }
        };

        Ok(has_permission)
    }

    async fn github_org(
        request: &Request<'_>,
        config: &Config,
        client_id: &str,
        client_secret: &str,
        publisher_team: &str,
    ) -> Result<Self, Error> {
        match github_org_identity(request, config, client_id, client_secret).await? {
            OrgIdentity::LegacyKey | OrgIdentity::CiKey => Ok(WriteAccess::ApiKey),
            OrgIdentity::User(user) => {
                require_membership(
                    request,
                    user.login(),
                    Group::Team(publisher_team.to_owned()),
                )
                .await?;
                Ok(WriteAccess::Publisher)
            }
        }
    }
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for WriteAccess {
    type Error = Error;

    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, Error> {
        let config = request
            .guard::<&State<Config>>()
            .await
            .expect("AuthMode was not configured");

        match &config.auth {
            AuthMode::Unauthenticated => format_err!("Invalid API key for write access")
                .status(Status::Unauthorized)
                .into(),
            AuthMode::ApiKey(key) => match_api_key(request, key, WriteAccess::ApiKey),
            AuthMode::DoubleApiKey { write, .. } => {
                match_api_key(request, write, WriteAccess::ApiKey)
            }
            AuthMode::GithubOAuth {
                client_id,
                client_secret,
            } => {
                let Some(token) = bearer_token(request) else {
                    return format_err!("Github auth required")
                        .status(Status::Unauthorized)
                        .into();
                };

                let result = verify_github_token(
                    &Client::new(),
                    &config.github_api_url,
                    token,
                    client_id,
                    client_secret,
                )
                .await;

                into_outcome(result.map(|user| {
                    AuditContext::of(request).set_actor(Actor::Github(user.clone()));
                    WriteAccess::Github(user)
                }))
            }
            AuthMode::GithubOrg {
                client_id,
                client_secret,
                publisher_team,
                ..
            } => into_outcome(
                WriteAccess::github_org(request, config, client_id, client_secret, publisher_team)
                    .await,
            ),
        }
    }
}
