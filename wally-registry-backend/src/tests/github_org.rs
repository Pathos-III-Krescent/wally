//! Tests for the `github-org` auth mode and audit logging. GitHub is replaced
//! by a local mock server, so these never touch the real GitHub API.

use figment::{
    Figment,
    providers::{Format, Toml},
};
use libwally::test_package::PackageBuilder;
use mockito::{Mock, Server, ServerGuard};
use rocket::{
    http::{Accept, ContentType, Header, Status},
    local::blocking::{Client, LocalResponse},
};
use serde_json::json;

use super::{Expectation, client_from_config, init_test_index_remote, test_config};
use crate::{auth::AuthMode, config::Config};

const CLIENT_ID: &str = "test-client-id";
const CLIENT_SECRET: &str = "test-client-secret";
const SERVER_TOKEN: &str = "test-server-token";
const CI_KEY: &str = "test-ci-key";

const MEMBER: &str = "member-token";
const PUBLISHER: &str = "publisher-token";
const OUTSIDER: &str = "outsider-token";

/// A fake GitHub where `member` is in the org, `publisher` is in the org and
/// the `wally-publishers` team, and `outsider` is in neither.
fn mock_github() -> (ServerGuard, Vec<Mock>) {
    let mut github = Server::new();
    let mut mocks = Vec::new();

    for (token, login, id) in [
        (MEMBER, "member", 1),
        (PUBLISHER, "publisher", 2),
        (OUTSIDER, "outsider", 3),
    ] {
        mocks.push(
            github
                .mock("GET", "/user")
                .match_header("authorization", format!("Bearer {}", token).as_str())
                .with_body(json!({ "login": login, "id": id }).to_string())
                .create(),
        );
    }

    mocks.push(
        github
            .mock(
                "POST",
                format!("/applications/{}/token", CLIENT_ID).as_str(),
            )
            .with_body(json!({ "id": 1, "app": { "client_id": CLIENT_ID } }).to_string())
            .create(),
    );

    // Membership must be checked with the server's token, not the user's.
    let server_auth = format!("Bearer {}", SERVER_TOKEN);
    for (path, status, body) in [
        ("/orgs/test-org/members/member", 204, ""),
        ("/orgs/test-org/members/publisher", 204, ""),
        ("/orgs/test-org/members/outsider", 404, ""),
        (
            "/orgs/test-org/teams/wally-publishers/memberships/publisher",
            200,
            r#"{"state":"active"}"#,
        ),
        (
            "/orgs/test-org/teams/wally-publishers/memberships/member",
            404,
            "",
        ),
        (
            "/orgs/test-org/teams/wally-publishers/memberships/outsider",
            404,
            "",
        ),
    ] {
        mocks.push(
            github
                .mock("GET", path)
                .match_header("authorization", server_auth.as_str())
                .with_status(status)
                .with_body(body)
                .create(),
        );
    }

    (github, mocks)
}

fn github_org_config(github: &ServerGuard) -> Config {
    let mut config = test_config(
        AuthMode::GithubOrg {
            client_id: CLIENT_ID.to_owned(),
            client_secret: CLIENT_SECRET.to_owned(),
            org: "test-org".to_owned(),
            reader_team: None,
            publisher_team: "wally-publishers".to_owned(),
            cache_ttl_secs: 60,
        },
        init_test_index_remote().unwrap(),
    );
    config.github_token = Some(SERVER_TOKEN.to_owned());
    config.github_api_url = github.url();
    config.ci_publish_key = Some(CI_KEY.to_owned());
    config
}

fn download<'c>(client: &'c Client, token: Option<&str>) -> LocalResponse<'c> {
    let mut request = client.get("/v1/package-contents/biff/minimal/0.1.0");
    if let Some(token) = token {
        request = request.header(Header::new("Authorization", format!("Bearer {}", token)));
    }
    request.dispatch()
}

fn publish<'c>(client: &'c Client, token: &str, package: &str) -> LocalResponse<'c> {
    client
        .post("/v1/publish")
        .header(Accept::JSON)
        .header(Header::new("Authorization", format!("Bearer {}", token)))
        .body(PackageBuilder::new(package).contents().data())
        .dispatch()
}

fn expect_json(status: Status) -> Expectation {
    Expectation {
        status,
        content_type: ContentType::JSON,
    }
}

#[test]
fn unauthenticated_read_denied() {
    let (github, _mocks) = mock_github();
    let client = client_from_config(github_org_config(&github));

    expect_json(Status::Unauthorized).assert(download(&client, None));
    expect_json(Status::Unauthorized).assert(download(&client, Some("not-a-real-token")));
    expect_json(Status::Unauthorized)
        .assert(client.get("/v1/package-search?query=minimal").dispatch());
}

#[test]
fn non_member_read_denied() {
    let (github, _mocks) = mock_github();
    let client = client_from_config(github_org_config(&github));

    expect_json(Status::Forbidden).assert(download(&client, Some(OUTSIDER)));
}

#[test]
fn member_read_allowed() {
    let (github, _mocks) = mock_github();
    let client = client_from_config(github_org_config(&github));

    Expectation {
        status: Status::Ok,
        content_type: ContentType::GZIP,
    }
    .assert(download(&client, Some(MEMBER)));

    expect_json(Status::Ok).assert(
        client
            .get("/v1/package-search?query=minimal")
            .header(Header::new("Authorization", format!("Bearer {}", MEMBER)))
            .dispatch(),
    );
}

#[test]
fn reader_team_limits_reads() {
    let (mut github, _mocks) = mock_github();
    let _readers = github
        .mock(
            "GET",
            "/orgs/test-org/teams/wally-readers/memberships/publisher",
        )
        .with_body(r#"{"state":"pending"}"#)
        .create();

    let mut config = github_org_config(&github);
    if let AuthMode::GithubOrg { reader_team, .. } = &mut config.auth {
        *reader_team = Some("wally-readers".to_owned());
    }
    let client = client_from_config(config);

    // Org membership isn't enough, and a pending team invite doesn't count.
    expect_json(Status::Forbidden).assert(download(&client, Some(PUBLISHER)));
}

#[test]
fn membership_is_cached() {
    let mut github = Server::new();
    let user = github
        .mock("GET", "/user")
        .with_body(r#"{"login":"member","id":1}"#)
        .expect(1)
        .create();
    let app = github
        .mock(
            "POST",
            format!("/applications/{}/token", CLIENT_ID).as_str(),
        )
        .with_body(json!({ "id": 1, "app": { "client_id": CLIENT_ID } }).to_string())
        .expect(1)
        .create();
    let member = github
        .mock("GET", "/orgs/test-org/members/member")
        .with_status(204)
        .expect(1)
        .create();

    let client = client_from_config(github_org_config(&github));
    for _ in 0..3 {
        assert_eq!(download(&client, Some(MEMBER)).status(), Status::Ok);
    }

    user.assert();
    app.assert();
    member.assert();
}

#[test]
fn non_publisher_publish_denied() {
    let (github, _mocks) = mock_github();
    let client = client_from_config(github_org_config(&github));

    expect_json(Status::Forbidden).assert(publish(&client, MEMBER, "biff/hello@1.0.0"));
    expect_json(Status::Forbidden).assert(publish(&client, OUTSIDER, "biff/hello@1.0.0"));
}

#[test]
fn publisher_publish_allowed() {
    let (github, _mocks) = mock_github();
    let client = client_from_config(github_org_config(&github));

    // Any scope is fine; scope owners files aren't used in this mode.
    expect_json(Status::Ok).assert(publish(&client, PUBLISHER, "biff/hello@1.0.0"));
    expect_json(Status::Ok).assert(publish(&client, PUBLISHER, "someone-else/hello@1.0.0"));
}

#[test]
fn ci_key_can_publish_but_not_read() {
    let (github, _mocks) = mock_github();
    let client = client_from_config(github_org_config(&github));

    expect_json(Status::Ok).assert(publish(&client, CI_KEY, "biff/hello@1.0.0"));
    expect_json(Status::Forbidden).assert(download(&client, Some(CI_KEY)));
}

#[test]
fn legacy_key_can_read_and_publish() {
    let (github, _mocks) = mock_github();
    let mut config = github_org_config(&github);
    config.legacy_api_key = Some("old-shared-key".to_owned());
    let client = client_from_config(config);

    assert_eq!(
        download(&client, Some("old-shared-key")).status(),
        Status::Ok
    );
    expect_json(Status::Ok).assert(publish(&client, "old-shared-key", "biff/hello@1.0.0"));
}

#[test]
fn audit_lines_written_without_secrets() {
    let (github, _mocks) = mock_github();
    let audit_dir = tempfile::tempdir().unwrap();
    let audit_path = audit_dir.path().join("audit.log");

    let mut config = github_org_config(&github);
    config.audit_log_path = Some(audit_path.to_string_lossy().into_owned());
    // Nothing listens here, so the webhook fails. Requests must still work.
    config.audit_webhook = Some("http://127.0.0.1:9/webhook".to_owned());
    let client = client_from_config(config);

    let response = client
        .get("/v1/package-contents/biff/minimal/0.1.0")
        .header(Header::new("Authorization", format!("Bearer {}", MEMBER)))
        .header(Header::new("X-Forwarded-For", "203.0.113.7, 10.0.0.2"))
        .header(Header::new("Wally-Version", "0.3.2"))
        .dispatch();
    assert_eq!(response.status(), Status::Ok);
    assert_eq!(
        download(&client, Some(OUTSIDER)).status(),
        Status::Forbidden
    );
    assert_eq!(download(&client, None).status(), Status::Unauthorized);
    assert_eq!(
        publish(&client, CI_KEY, "biff/hello@1.0.0").status(),
        Status::Ok
    );

    let contents = std::fs::read_to_string(&audit_path).unwrap();
    for secret in [
        MEMBER,
        OUTSIDER,
        CI_KEY,
        SERVER_TOKEN,
        CLIENT_SECRET,
        "Bearer",
    ] {
        assert!(
            !contents.contains(secret),
            "audit log contains {:?}",
            secret
        );
    }

    let lines: Vec<serde_json::Value> = contents
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines.len(), 4);

    let download_line = &lines[0];
    assert_eq!(download_line["event"], "package.download");
    assert_eq!(download_line["github_username"], "member");
    assert_eq!(download_line["github_user_id"], 1);
    assert_eq!(download_line["package"], "biff/minimal");
    assert_eq!(download_line["version"], "0.1.0");
    assert_eq!(download_line["status"], 200);
    assert_eq!(download_line["source_ip"], "203.0.113.7");
    assert_eq!(download_line["result"], "allowed");
    assert_eq!(download_line["wally_version"], "0.3.2");
    // Whole seconds in UTC, e.g. 2026-11-02T02:10:00Z
    let timestamp = download_line["timestamp"].as_str().unwrap();
    assert!(
        timestamp.len() == 20 && timestamp.ends_with('Z'),
        "{}",
        timestamp
    );

    assert_eq!(lines[1]["github_username"], "outsider");
    assert_eq!(lines[1]["status"], 403);
    assert_eq!(lines[1]["result"], "denied");
    assert!(
        lines[1]["reason"]
            .as_str()
            .unwrap()
            .contains("not a member")
    );

    assert_eq!(lines[2]["github_username"], "anonymous");
    assert_eq!(lines[2]["github_user_id"], serde_json::Value::Null);
    assert_eq!(lines[2]["result"], "denied");

    assert_eq!(lines[3]["event"], "package.publish");
    assert_eq!(lines[3]["github_username"], "ci-key");
    assert_eq!(lines[3]["package"], "biff/hello");
    assert_eq!(lines[3]["version"], "1.0.0");
    assert_eq!(lines[3]["result"], "allowed");
}

#[test]
fn release_config_requires_auth() {
    // Rocket.toml must not ship a default auth mode; production has to set one.
    let figment = Figment::from(rocket::Config::default())
        .merge(Toml::file("Rocket.toml").nested())
        .select("release");

    let error = figment
        .extract::<Config>()
        .err()
        .expect("auth should be required");
    assert!(error.to_string().contains("auth"), "{}", error);
}

#[test]
fn invalid_github_org_config_refused() {
    let github = Server::new();

    let mut config = github_org_config(&github);
    config.github_token = None;
    assert!(config.validate().is_err());

    let mut config = github_org_config(&github);
    if let AuthMode::GithubOrg { cache_ttl_secs, .. } = &mut config.auth {
        *cache_ttl_secs = 61;
    }
    assert!(config.validate().is_err());

    let mut config = test_config(AuthMode::Unauthenticated, init_test_index_remote().unwrap());
    config.ci_publish_key = Some(CI_KEY.to_owned());
    assert!(config.validate().is_err());
}

#[test]
fn empty_env_values_are_unset() {
    let figment = Figment::new().merge(Toml::string(
        r#"
        index_url = "https://example.com/index"
        auth = { type = "unauthenticated" }
        storage = { type = "local" }
        github_token = ""
        ci_publish_key = "  "
        audit_webhook = ""
        "#,
    ));

    let config: Config = figment.extract().unwrap();
    assert!(config.github_token.is_none());
    assert!(config.ci_publish_key.is_none());
    assert!(config.audit_webhook.is_none());
}

#[test]
fn example_env_auth_parses() {
    // Parse the same way WALLY_* environment variables are parsed in production.
    let example = std::fs::read_to_string("../.example.env").unwrap();
    let value = example
        .lines()
        .find_map(|line| line.strip_prefix("WALLY_auth="))
        .expect(".example.env should set WALLY_auth");

    let auth: AuthMode = value
        .parse::<figment::value::Value>()
        .unwrap()
        .deserialize()
        .unwrap();

    match auth {
        AuthMode::GithubOrg {
            org,
            reader_team,
            publisher_team,
            cache_ttl_secs,
            ..
        } => {
            assert_eq!(org, "Pathos-III-Krescent");
            assert_eq!(reader_team, None);
            assert_eq!(publisher_team, "wally-publishers");
            assert_eq!(cache_ttl_secs, 60);
        }
        other => panic!("expected github-org auth, got {:?}", other),
    }
}
