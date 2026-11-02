//! Writes one JSON line per API request recording who did what and whether it
//! was allowed. Lines go to stdout and, optionally, an append-only file.
//!
//! Never put tokens, API keys or headers other than the ones below in here.

use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::Path,
    sync::{Mutex, OnceLock},
};

use anyhow::Context;
use libwally::package_id::PackageId;
use rocket::{
    Request, Response,
    fairing::{Fairing, Info, Kind},
    request::{FromRequest, Outcome},
};
use serde::Serialize;
use serde_json::json;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::github::GithubInfo;

/// Who made a request.
pub enum Actor {
    Github(GithubInfo),
    ApiKey,
    CiKey,
    LegacyKey,
}

/// What we learned about a request while handling it. Filled in by request
/// guards and handlers, then read by [`AuditLog`] once the response is ready.
#[derive(Default)]
pub struct AuditContext {
    actor: OnceLock<Actor>,
    package: OnceLock<PackageId>,
    reason: OnceLock<String>,
}

impl AuditContext {
    pub fn of<'r>(request: &'r Request<'_>) -> &'r AuditContext {
        request.local_cache(AuditContext::default)
    }

    pub fn set_actor(&self, actor: Actor) {
        let _ = self.actor.set(actor);
    }

    pub fn set_package(&self, package_id: &PackageId) {
        let _ = self.package.set(package_id.clone());
    }

    pub fn set_reason(&self, reason: &str) {
        let _ = self.reason.set(reason.to_owned());
    }
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for &'r AuditContext {
    type Error = ();

    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, ()> {
        Outcome::Success(AuditContext::of(request))
    }
}

#[derive(Serialize)]
struct AuditLine<'a> {
    event: &'static str,
    timestamp: String,
    github_username: &'a str,
    github_user_id: Option<u64>,
    package: Option<String>,
    version: Option<String>,
    status: u16,
    source_ip: Option<String>,
    result: &'static str,
    reason: Option<&'a str>,
    wally_version: Option<&'a str>,
}

pub struct AuditLog {
    file: Option<Mutex<File>>,
    webhook: Option<String>,
    client: reqwest::Client,
}

impl AuditLog {
    pub fn new(path: Option<&Path>, webhook: Option<String>) -> anyhow::Result<Self> {
        let file = match path {
            Some(path) => {
                let file = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .with_context(|| format!("could not open audit log {}", path.display()))?;
                Some(Mutex::new(file))
            }
            None => None,
        };

        Ok(Self {
            file,
            webhook,
            client: reqwest::Client::new(),
        })
    }

    fn write(&self, line: &str) {
        println!("{}", line);

        if let Some(file) = &self.file {
            let mut file = file.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Err(err) = writeln!(file, "{}", line) {
                eprintln!("Could not write to audit log: {}", err);
            }
        }
    }

    /// Sends a line to the Discord webhook in the background. A failing
    /// webhook must never fail the request.
    fn notify(&self, line: &str) {
        let Some(webhook) = self.webhook.clone() else {
            return;
        };

        let content: String = format!("```json\n{}\n```", line)
            .chars()
            .take(2000)
            .collect();
        let request = self
            .client
            .post(webhook)
            .json(&json!({ "content": content }));

        tokio::spawn(async move {
            if let Err(err) = request.send().await.and_then(|r| r.error_for_status()) {
                // The error contains the webhook URL, which is a secret.
                eprintln!("Could not send audit webhook: {}", err.without_url());
            }
        });
    }
}

#[rocket::async_trait]
impl Fairing for AuditLog {
    fn info(&self) -> Info {
        Info {
            name: "Audit log",
            kind: Kind::Response,
        }
    }

    async fn on_response<'r>(&self, request: &'r Request<'_>, response: &mut Response<'r>) {
        let route_name = request.route().and_then(|route| route.name.as_deref());
        let event = match route_name {
            Some("package_contents") => "package.download",
            Some("package_info") => "package.metadata",
            Some("package_search") => "package.search",
            Some("publish") => "package.publish",
            _ => return,
        };

        let context = AuditContext::of(request);
        // Paths look like /v1/package-contents/<scope>/<name>/<version>.
        let segment = |n| request.routed_segment(n).map(str::to_owned);
        let package =
            |scope: Option<String>, name: Option<String>| Some(format!("{}/{}", scope?, name?));
        let (package, version) = match (event, context.package.get()) {
            ("package.download", _) => (package(segment(2), segment(3)), segment(4)),
            ("package.metadata", _) => (package(segment(2), segment(3)), None),
            ("package.publish", Some(package_id)) => (
                Some(package_id.name().to_string()),
                Some(package_id.version().to_string()),
            ),
            _ => (None, None),
        };

        let (github_username, github_user_id) = match context.actor.get() {
            Some(Actor::Github(info)) => (info.login(), Some(*info.id())),
            Some(Actor::ApiKey) => ("api-key", None),
            Some(Actor::CiKey) => ("ci-key", None),
            Some(Actor::LegacyKey) => ("legacy-key", None),
            None => ("anonymous", None),
        };

        // Traefik sets X-Forwarded-For; the left-most entry is the client.
        let source_ip = request
            .headers()
            .get_one("X-Forwarded-For")
            .and_then(|ips| ips.split(',').next())
            .map(|ip| ip.trim().to_owned())
            .or_else(|| request.client_ip().map(|ip| ip.to_string()));

        let status = response.status();
        let result = match status.code {
            200..=299 => "allowed",
            401 | 403 => "denied",
            _ => "error",
        };

        let line = AuditLine {
            event,
            timestamp: OffsetDateTime::now_utc()
                .replace_nanosecond(0)
                .ok()
                .and_then(|now| now.format(&Rfc3339).ok())
                .unwrap_or_default(),
            github_username,
            github_user_id,
            package,
            version,
            status: status.code,
            source_ip,
            result,
            reason: context.reason.get().map(String::as_str),
            wally_version: request.headers().get_one("Wally-Version"),
        };

        let line = match serde_json::to_string(&line) {
            Ok(line) => line,
            Err(err) => {
                eprintln!("Could not serialize audit line: {}", err);
                return;
            }
        };

        self.write(&line);

        if event == "package.publish" || result == "denied" {
            self.notify(&line);
        }
    }
}
