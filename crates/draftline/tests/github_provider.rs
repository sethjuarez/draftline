//! GitHubProvider against a fake GitHub API (`github` feature).

use draftline::github::{GitHubProvider, GitHubRepo};
use draftline::tauri_contract::{
    github_repo_access, github_repo_access_on_enterprise_host, GitHubRepoAccessRequest,
    TauriCommandError,
};
use draftline::{DraftlineError, PullRequestProvider, PullRequestRequest};
use httpmock::prelude::*;
use serde_json::json;

const TOKEN: &str = "test-token";

fn request() -> PullRequestRequest {
    PullRequestRequest {
        remote: "origin".to_string(),
        head: "cutready/launch-demo".to_string(),
        base: "main".to_string(),
        title: "Launch demo".to_string(),
        body: "Ready for review".to_string(),
    }
}

fn provider(server: &MockServer) -> GitHubProvider {
    GitHubProvider::for_remote_url("https://github.com/contoso/app.git", TOKEN)
        .unwrap()
        .with_api_base(server.base_url())
        .with_user_agent("CutReady")
}

#[test]
fn find_open_asks_for_the_owners_head_branch_with_the_hosts_token() {
    let server = MockServer::start();
    let lookup = server.mock(|when, then| {
        when.method(GET)
            .path("/repos/contoso/app/pulls")
            .query_param("state", "open")
            .query_param("head", "contoso:cutready/launch-demo")
            .query_param("base", "main")
            .header("authorization", format!("Bearer {TOKEN}"))
            .header("accept", "application/vnd.github+json")
            .header("x-github-api-version", "2022-11-28")
            .header("user-agent", "CutReady");
        then.status(200).json_body(json!([
            { "number": 7, "html_url": "https://github.com/contoso/app/pull/7" },
            { "number": 3, "html_url": "https://github.com/contoso/app/pull/3" }
        ]));
    });

    let found = provider(&server).find_open(&request()).unwrap().unwrap();
    lookup.assert();
    assert_eq!(found.number, 7);
    assert_eq!(found.url, "https://github.com/contoso/app/pull/7");
}

#[test]
fn find_open_is_none_without_an_open_pull_request() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/repos/contoso/app/pulls");
        then.status(200).json_body(json!([]));
    });
    assert_eq!(provider(&server).find_open(&request()).unwrap(), None);
}

#[test]
fn create_opens_a_pull_request() {
    let server = MockServer::start();
    let create = server.mock(|when, then| {
        when.method(POST)
            .path("/repos/contoso/app/pulls")
            .header("authorization", format!("Bearer {TOKEN}"))
            .json_body(json!({
                "title": "Launch demo",
                "head": "contoso:cutready/launch-demo",
                "base": "main",
                "body": "Ready for review"
            }));
        then.status(201).json_body(
            json!({ "number": 12, "html_url": "https://github.com/contoso/app/pull/12" }),
        );
    });

    let pull = provider(&server).create(&request()).unwrap();
    create.assert();
    assert_eq!(pull.number, 12);
}

fn create_error(status: u16, body: serde_json::Value, rate_limited: bool) -> DraftlineError {
    create_error_with(
        status,
        body,
        rate_limited.then_some(("x-ratelimit-remaining", "0")),
    )
}

fn create_error_with(
    status: u16,
    body: serde_json::Value,
    header: Option<(&str, &str)>,
) -> DraftlineError {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(POST).path("/repos/contoso/app/pulls");
        let then = then.status(status).json_body(body);
        if let Some((name, value)) = header {
            then.header(name, value);
        }
    });
    provider(&server).create(&request()).unwrap_err()
}

fn code(error: DraftlineError) -> (String, Option<serde_json::Value>, String) {
    let error = TauriCommandError::from(error);
    (error.code, error.details, error.message)
}

#[test]
fn github_failures_map_to_stable_codes() {
    let (code_401, _, _) = code(create_error(
        401,
        json!({ "message": "Bad credentials" }),
        false,
    ));
    assert_eq!(code_401, "github_unauthorized");

    let (code_403, details, message) = code(create_error(
        403,
        json!({ "message": "Must have push access to repository" }),
        false,
    ));
    assert_eq!(code_403, "github_no_push_access");
    assert_eq!(details, Some(json!({ "repository": "contoso/app" })));
    assert!(message.contains("Must have push access"), "{message}");

    let (code_404, details, _) = code(create_error(404, json!({ "message": "Not Found" }), false));
    assert_eq!(code_404, "github_repository_not_found");
    assert_eq!(details, Some(json!({ "repository": "contoso/app" })));

    let (code_422, _, message) = code(create_error(
        422,
        json!({
            "message": "Validation Failed",
            "errors": [{ "message": "No commits between main and cutready/launch-demo" }]
        }),
        false,
    ));
    assert_eq!(code_422, "pull_request_failed");
    assert!(
        message.contains("Validation Failed (No commits between"),
        "{message}"
    );

    // A rate limit isn't a permissions problem.
    let (code_limited, _, _) = code(create_error(
        403,
        json!({ "message": "API rate limit exceeded" }),
        true,
    ));
    assert_eq!(code_limited, "pull_request_failed");

    // A secondary rate limit leaves the primary quota alone.
    let (code_secondary, _, _) = code(create_error_with(
        403,
        json!({ "message": "You have exceeded a secondary rate limit" }),
        Some(("retry-after", "60")),
    ));
    assert_eq!(code_secondary, "pull_request_failed");

    // A 403 that doesn't say the token can't push isn't reported as that.
    let (code_policy, _, _) = code(create_error(
        403,
        json!({ "message": "Organization policy blocks this action" }),
        false,
    ));
    assert_eq!(code_policy, "pull_request_failed");
    let (code_scope, _, _) = code(create_error(
        403,
        json!({ "message": "Resource not accessible by personal access token" }),
        false,
    ));
    assert_eq!(code_scope, "pull_request_failed");
    let (code_sso, _, _) = code(create_error(
        403,
        json!({ "message": "Resource protected by organization SAML enforcement" }),
        false,
    ));
    assert_eq!(code_sso, "github_unauthorized");
}

#[test]
fn a_pull_request_from_a_fork_targets_the_parent_with_the_fork_owners_head() {
    let server = MockServer::start();
    let lookup = server.mock(|when, then| {
        when.method(GET)
            .path("/repos/upstream/app/pulls")
            .query_param("head", "me:cutready/launch-demo");
        then.status(200).json_body(json!([]));
    });
    let create = server.mock(|when, then| {
        when.method(POST)
            .path("/repos/upstream/app/pulls")
            .json_body_partial(r#"{ "head": "me:cutready/launch-demo" }"#);
        then.status(201).json_body(
            json!({ "number": 3, "html_url": "https://github.com/upstream/app/pull/3" }),
        );
    });
    let mut provider = GitHubProvider::new(GitHubRepo::new("upstream", "app"), TOKEN)
        .unwrap()
        .with_api_base(server.base_url())
        .with_head_owner("me");
    assert_eq!(provider.find_open(&request()).unwrap(), None);
    assert_eq!(provider.create(&request()).unwrap().number, 3);
    lookup.assert();
    create.assert();
}

#[test]
fn create_uses_a_pull_request_someone_else_just_opened() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(POST).path("/repos/contoso/app/pulls");
        then.status(422).json_body(json!({
            "message": "Validation Failed",
            "errors": [{ "message": "A pull request already exists for contoso:cutready/launch-demo." }]
        }));
    });
    server.mock(|when, then| {
        when.method(GET).path("/repos/contoso/app/pulls");
        then.status(200).json_body(
            json!([{ "number": 9, "html_url": "https://github.com/contoso/app/pull/9" }]),
        );
    });
    assert_eq!(provider(&server).create(&request()).unwrap().number, 9);
}

#[test]
fn enterprise_remotes_parse_on_their_own_host_only() {
    use draftline::github::{enterprise_api_base, parse_remote_url_on};
    let repo = Some(GitHubRepo::new("contoso", "app"));
    for url in [
        "https://ghe.example.com/contoso/app.git",
        "https://user:token@ghe.example.com/contoso/app",
        "git@ghe.example.com:contoso/app.git",
        "ssh://git@ghe.example.com:2222/contoso/app.git",
    ] {
        assert_eq!(parse_remote_url_on(url, "ghe.example.com"), repo, "{url}");
    }
    for url in [
        "https://github.com/contoso/app",
        "http://ghe.example.com/contoso/app",
        "https://ghe.example.com.evil.test/contoso/app",
    ] {
        assert_eq!(parse_remote_url_on(url, "ghe.example.com"), None, "{url}");
    }
    assert_eq!(
        enterprise_api_base("ghe.example.com").unwrap(),
        "https://ghe.example.com/api/v3"
    );
    assert!(enterprise_api_base("ghe.example.com/x").is_err());
    assert!(enterprise_api_base("").is_err());
}

#[test]
fn a_lookup_refused_by_github_is_a_sign_in_problem() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/repos/contoso/app/pulls");
        then.status(403)
            .json_body(json!({ "message": "Resource protected by organization SAML enforcement" }));
    });
    let error = provider(&server).find_open(&request()).unwrap_err();
    assert!(
        matches!(error, DraftlineError::GitHubUnauthorized(_)),
        "{error:?}"
    );
}

#[test]
fn repo_access_reports_push_permission_default_branch_and_fork_parent() {
    let server = MockServer::start();
    let lookup = server.mock(|when, then| {
        when.method(GET)
            .path("/repos/contoso/app")
            .header("authorization", format!("Bearer {TOKEN}"));
        then.status(200).json_body(json!({
            "full_name": "contoso/app",
            "default_branch": "trunk",
            "private": true,
            "fork": true,
            "permissions": { "admin": false, "push": true, "pull": true },
            "parent": { "full_name": "upstream/app" }
        }));
    });

    let access = provider(&server).repo_access().unwrap();
    lookup.assert();
    assert_eq!(access.repository.full_name(), "contoso/app");
    assert_eq!(access.default_branch, "trunk");
    assert!(access.can_push && access.private);
    assert_eq!(
        access.fork_parent,
        Some(GitHubRepo {
            owner: "upstream".to_string(),
            repo: "app".to_string(),
        })
    );
}

#[test]
fn contract_reads_repo_access_and_reports_read_only_tokens() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET)
            .path("/api/v3/repos/contoso/app")
            .header("authorization", format!("Bearer {TOKEN}"));
        then.status(200).json_body(json!({
            "full_name": "Contoso/App",
            "default_branch": "main",
            "permissions": { "push": false, "pull": true }
        }));
    });
    // A GitHub Enterprise Server remote; a loopback host is plain HTTP in tests.
    let host = server.address().to_string();
    let request: GitHubRepoAccessRequest = serde_json::from_value(json!({
        "remote_url": format!("https://someone:secret@{host}/contoso/app.git"),
        "token": TOKEN,
    }))
    .unwrap();
    let debug = format!("{request:?}");
    assert!(!debug.contains(TOKEN) && !debug.contains("secret"));

    let access = github_repo_access_on_enterprise_host(request, &host).unwrap();
    assert_eq!(access.repository.full_name(), "Contoso/App");
    assert!(!access.can_push);
    assert_eq!(access.fork_parent, None);
    let value = serde_json::to_value(&access).unwrap();
    assert_eq!(
        value["repository"],
        json!({ "owner": "Contoso", "repo": "App" })
    );
    assert_eq!(value["can_push"], json!(false));
}

#[test]
fn contract_refuses_a_missing_token_and_a_non_github_remote() {
    let error = github_repo_access(GitHubRepoAccessRequest {
        remote_url: "https://github.com/contoso/app".to_string(),
        token: String::new(),
    })
    .unwrap_err();
    assert_eq!(TauriCommandError::from(error).code, "github_unauthorized");

    let error = github_repo_access(GitHubRepoAccessRequest {
        remote_url: "https://user:secret@dev.azure.com/o/p/_git/r".to_string(),
        token: TOKEN.to_string(),
    })
    .unwrap_err();
    let error = TauriCommandError::from(error);
    assert_eq!(error.code, "pull_request_failed");
    assert!(!error.message.contains("secret"), "{}", error.message);

    // A github.com remote can't send the token to another host.
    let error = github_repo_access_on_enterprise_host(
        GitHubRepoAccessRequest {
            remote_url: "https://github.com/contoso/app".to_string(),
            token: TOKEN.to_string(),
        },
        "ghe.example.com",
    )
    .unwrap_err();
    assert_eq!(TauriCommandError::from(error).code, "pull_request_failed");
    let error = github_repo_access_on_enterprise_host(
        GitHubRepoAccessRequest {
            remote_url: "https://ghe.example.com/contoso/app".to_string(),
            token: TOKEN.to_string(),
        },
        "ghe.example.com/evil?",
    )
    .unwrap_err();
    assert_eq!(TauriCommandError::from(error).code, "pull_request_failed");
}

#[test]
fn inspect_repository_adds_github_access_for_a_local_repository() {
    use draftline::tauri_contract::{
        inspect_repository, inspect_repository_with_context, DraftlineCommandContext,
        InspectRepositoryRequest,
    };
    let server = MockServer::start();
    let host = server.address().to_string();
    server.mock(|when, then| {
        when.method(GET)
            .path("/api/v3/repos/contoso/app")
            .header("authorization", format!("Bearer {TOKEN}"));
        then.status(200).json_body(json!({
            "full_name": "contoso/app",
            "default_branch": "main",
            "permissions": { "push": true }
        }));
    });
    let temp = tempfile::tempdir().unwrap();
    let repo = git2::Repository::init(temp.path()).unwrap();
    repo.remote("upstream", "https://example.com/other.git")
        .unwrap();
    repo.remote("origin", &format!("https://{host}/contoso/app.git"))
        .unwrap();
    let request = |token: Option<&str>| InspectRepositoryRequest {
        location: temp.path().to_str().unwrap().to_string(),
        target_folder: None,
        timeout_ms: None,
        github_token: token.map(str::to_string),
    };

    let mut context = DraftlineCommandContext::new().with_github_enterprise_host(host.clone());
    let result = inspect_repository_with_context(&mut context, request(Some(TOKEN))).unwrap();
    let access = result.github.unwrap();
    assert!(access.can_push);
    assert_eq!(result.github_error, None);
    assert!(result.inspection.is_repository);

    // A refused token doesn't sink the rest of the inspection.
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/api/v3/repos/contoso/app");
        then.status(401)
            .json_body(json!({ "message": "Bad credentials" }));
    });
    repo.remote_set_url(
        "origin",
        &format!("https://{}/contoso/app.git", server.address()),
    )
    .unwrap();
    let mut context =
        DraftlineCommandContext::new().with_github_enterprise_host(server.address().to_string());
    let result = inspect_repository_with_context(&mut context, request(Some(TOKEN))).unwrap();
    assert_eq!(result.github, None);
    assert_eq!(result.github_error.unwrap().code, "github_unauthorized");

    // No token, or no github.com remote: nothing asked.
    let result = inspect_repository(request(None)).unwrap();
    assert_eq!((result.github, result.github_error), (None, None));
    let result = inspect_repository(request(Some(TOKEN))).unwrap();
    assert_eq!((result.github, result.github_error), (None, None));
}
