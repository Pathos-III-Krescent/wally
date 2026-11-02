//! Talks to the GitHub API to verify user tokens and check org/team
//! membership, caching the answers for a short time to stay under GitHub's
//! rate limits.

use std::{collections::HashMap, time::Duration};

use anyhow::{anyhow, format_err};
use moka::sync::Cache;
use reqwest::{Client, StatusCode, redirect};
use rocket::http::Status;
use serde::Deserialize;

use crate::error::{ApiErrorStatus, Error};

#[derive(Clone, Deserialize)]
pub struct GithubInfo {
    login: String,
    id: u64,
}

impl GithubInfo {
    pub fn login(&self) -> &str {
        &self.login
    }

    pub fn id(&self) -> &u64 {
        &self.id
    }
}

#[derive(Deserialize)]
#[allow(unused)] // Variables are (currently) not accessed but ensure they are present during json parsing
struct ValidatedGithubApp {
    client_id: String,
}

#[derive(Deserialize)]
#[allow(unused)] // Variables are (currently) not accessed but ensure they are present during json parsing
struct ValidatedGithubInfo {
    id: u64,
    app: ValidatedGithubApp,
}

#[derive(Deserialize)]
struct TeamMembership {
    state: String,
}

/// Checks that `token` is a valid GitHub token issued to our OAuth app and
/// returns the user it belongs to.
pub async fn verify_github_token(
    client: &Client,
    api_url: &str,
    token: &str,
    client_id: &str,
    client_secret: &str,
) -> Result<GithubInfo, Error> {
    let response = client
        .get(format!("{}/user", api_url))
        .header("accept", "application/json")
        .header("user-agent", "wally")
        .bearer_auth(token)
        .send()
        .await;

    let github_info = match response {
        Err(err) => {
            return Err(format_err!(err).status(Status::InternalServerError));
        }
        Ok(response) => match response.json::<GithubInfo>().await {
            Err(err) => {
                return Err(format_err!("Github auth failed: {}", err).status(Status::Unauthorized));
            }
            Ok(github_info) => github_info,
        },
    };

    let mut body = HashMap::new();
    body.insert("access_token", token);

    let response = client
        .post(format!("{}/applications/{}/token", api_url, client_id))
        .header("accept", "application/json")
        .header("user-agent", "wally")
        .basic_auth(client_id, Some(client_secret))
        .json(&body)
        .send()
        .await;

    let validated_github_info = match response {
        Err(err) => {
            return Err(format_err!(err).status(Status::InternalServerError));
        }
        Ok(response) => {
            // If a code 422 (unprocessable entity) is returned, it's a sign of
            // auth failure. Otherwise, we don't know what happened!
            // https://docs.github.com/en/rest/apps/oauth-applications#check-a-token--status-codes
            match response.status() {
                StatusCode::OK => response.json::<ValidatedGithubInfo>().await,
                StatusCode::UNPROCESSABLE_ENTITY => {
                    return Err(anyhow!("GitHub auth was invalid").status(Status::Unauthorized));
                }
                status => {
                    return Err(format_err!("Github auth failed because: {}", status)
                        .status(Status::UnprocessableEntity));
                }
            }
        }
    };

    match validated_github_info {
        Err(err) => Err(format_err!("Github auth failed: {}", err).status(Status::Unauthorized)),
        Ok(_) => Ok(github_info),
    }
}

/// A group of GitHub users that can be granted access.
#[derive(Clone, Hash, PartialEq, Eq)]
pub enum Group {
    Org,
    Team(String),
}

/// Verifies users of the `github-org` auth mode. Holds the caches, so there is
/// one of these per server.
pub struct GithubOrgAuth {
    client: Client,
    api_url: String,
    server_token: String,
    org: String,
    users: Cache<String, GithubInfo>,
    memberships: Cache<(String, Group), bool>,
}

impl GithubOrgAuth {
    pub fn new(api_url: &str, server_token: &str, org: &str, cache_ttl: Duration) -> Self {
        Self {
            // A redirect from the membership endpoint means our token can't
            // see private members, so treat it as an error instead of following it.
            client: Client::builder()
                .redirect(redirect::Policy::none())
                .build()
                .expect("could not build HTTP client"),
            api_url: api_url.trim_end_matches('/').to_owned(),
            server_token: server_token.to_owned(),
            org: org.to_owned(),
            users: Cache::builder()
                .max_capacity(10_000)
                .time_to_live(cache_ttl)
                .build(),
            memberships: Cache::builder()
                .max_capacity(10_000)
                .time_to_live(cache_ttl)
                .build(),
        }
    }

    /// Returns who `token` belongs to, or an error if it isn't a valid token
    /// for our OAuth app.
    pub async fn user(
        &self,
        token: &str,
        client_id: &str,
        client_secret: &str,
    ) -> Result<GithubInfo, Error> {
        if let Some(info) = self.users.get(token) {
            return Ok(info);
        }

        let info =
            verify_github_token(&self.client, &self.api_url, token, client_id, client_secret)
                .await?;
        self.users.insert(token.to_owned(), info.clone());
        Ok(info)
    }

    /// Checks whether `login` is an active member of `group`, using the
    /// server's own token so users only need the `read:user` scope.
    pub async fn is_member(&self, login: &str, group: &Group) -> Result<bool, Error> {
        let key = (login.to_lowercase(), group.clone());
        if let Some(is_member) = self.memberships.get(&key) {
            return Ok(is_member);
        }

        let url = match group {
            Group::Org => format!("{}/orgs/{}/members/{}", self.api_url, self.org, login),
            Group::Team(team) => format!(
                "{}/orgs/{}/teams/{}/memberships/{}",
                self.api_url, self.org, team, login
            ),
        };

        let response = self
            .client
            .get(url)
            .header("accept", "application/vnd.github+json")
            .header("user-agent", "wally")
            .bearer_auth(&self.server_token)
            .send()
            .await
            .map_err(|err| format_err!(err).status(Status::InternalServerError))?;

        // https://docs.github.com/en/rest/orgs/members#check-organization-membership-for-a-user
        // https://docs.github.com/en/rest/teams/members#get-team-membership-for-a-user
        let is_member = match (group, response.status()) {
            (_, StatusCode::NOT_FOUND) => false,
            (Group::Org, StatusCode::NO_CONTENT) => true,
            (Group::Team(_), StatusCode::OK) => {
                let membership = response
                    .json::<TeamMembership>()
                    .await
                    .map_err(|err| format_err!(err).status(Status::InternalServerError))?;
                membership.state == "active"
            }
            (_, status) => {
                return Err(format_err!(
                    "Could not check GitHub membership ({}). Is the registry's github_token allowed to read org members?",
                    status
                )
                .status(Status::InternalServerError));
            }
        };

        self.memberships.insert(key, is_member);
        Ok(is_member)
    }
}
