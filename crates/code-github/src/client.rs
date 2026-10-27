//! The REST calls roll makes. Each call runs as the App (JWT) or as one installation (token),
//! follows `Link: rel="next"` pagination, and respects rate limits: it records
//! `x-ratelimit-*`, waits out `retry-after` and secondary limits when the wait is short, and
//! fails with [`Error::RateLimited`] when it isn't.

use crate::auth::{AppAuth, InstallationToken, parse_timestamp, unix_now};
use crate::config::percent_encode;
use crate::model::{
    Branch, CheckRun, CheckRunRequest, Comment, Installation, Issue, Label, PullRequest, Repository,
};
use crate::{AppConfig, Error, Result};
use parking_lot::Mutex;
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, LINK, RETRY_AFTER, USER_AGENT};
use reqwest::{Method, StatusCode};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::Duration;
use tracing::{debug, warn};

const API_VERSION: &str = "2022-11-28";

#[derive(Clone, Debug)]
pub struct ClientOptions {
    /// Retries after a rate limit, a 5xx or a revoked token.
    pub max_retries: u32,
    /// The longest wait for a rate limit to reset before giving up with `RateLimited`.
    pub max_backoff: Duration,
    /// First retry delay after a 5xx; doubles each time.
    pub server_error_backoff: Duration,
    /// What a secondary limit without `retry-after` waits (GitHub asks for at least a minute).
    pub secondary_backoff: Duration,
    /// Safety cap on pages fetched per list call.
    pub max_pages: u32,
    pub user_agent: String,
    pub timeout: Duration,
}

impl Default for ClientOptions {
    fn default() -> Self {
        ClientOptions {
            max_retries: 3,
            max_backoff: Duration::from_secs(60),
            server_error_backoff: Duration::from_millis(500),
            secondary_backoff: Duration::from_secs(60),
            max_pages: 100,
            user_agent: "roll-code".into(),
            timeout: Duration::from_secs(30),
        }
    }
}

/// The latest `x-ratelimit-*` values seen for one identity.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RateLimit {
    pub limit: Option<u64>,
    pub remaining: Option<u64>,
    /// Unix seconds when the window resets.
    pub reset: Option<u64>,
}

/// Credentials for `git fetch` / `git push` over HTTPS as an installation.
#[derive(Clone, Debug)]
pub struct GitCredentials {
    /// `https://github.com/<owner>/<repo>.git` (no credentials in it).
    pub url: String,
    /// Always `x-access-token`.
    pub username: String,
    pub password: String,
    /// Unix seconds; fetch fresh credentials after this.
    pub expires_at: u64,
}

impl std::fmt::Debug for GitHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitHub")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

/// What a branch's protection means for roll pushing to it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BranchProtection {
    pub protected: bool,
    /// The protection details were readable (needs `administration: read`).
    pub details: bool,
    /// Pushes are restricted to listed users, teams and apps.
    pub restricts_pushes: bool,
    /// Only this App may push: the recommended setup, since roll is the landing authority.
    pub only_roll_pushes: bool,
    /// This App is in the push allow-list (or pushes aren't restricted).
    pub roll_can_push: bool,
    pub allows_force_pushes: bool,
}

/// Which credential a request runs with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Who {
    App,
    Installation(u64),
}

struct Raw {
    body: bytes::Bytes,
    next: Option<String>,
}

pub struct GitHub {
    config: AppConfig,
    auth: AppAuth,
    http: reqwest::Client,
    opts: ClientOptions,
    rates: Mutex<HashMap<Who, RateLimit>>,
}

impl GitHub {
    pub fn new(config: AppConfig) -> Result<Self> {
        Self::with_options(config, ClientOptions::default())
    }

    pub fn with_options(config: AppConfig, opts: ClientOptions) -> Result<Self> {
        let auth = AppAuth::new(&config)?;
        let http = reqwest::Client::builder().timeout(opts.timeout).build()?;
        Ok(GitHub {
            config,
            auth,
            http,
            opts,
            rates: Mutex::new(HashMap::new()),
        })
    }

    pub fn config(&self) -> &AppConfig {
        &self.config
    }

    pub fn auth(&self) -> &AppAuth {
        &self.auth
    }

    /// The last rate-limit headers seen for an installation (`None`: the App's own JWT budget).
    pub fn rate_limit(&self, installation: Option<u64>) -> Option<RateLimit> {
        let who = installation.map_or(Who::App, Who::Installation);
        self.rates.lock().get(&who).copied()
    }

    // ── App ──

    pub async fn list_installations(&self) -> Result<Vec<Installation>> {
        self.list(Who::App, "/app/installations?per_page=100", None)
            .await
    }

    /// A token for `installation`, from the cache when it has more than 5 minutes left.
    pub async fn installation_token(&self, installation: u64) -> Result<InstallationToken> {
        if let Some(t) = self.auth.cached_token(installation, unix_now()) {
            return Ok(t);
        }
        #[derive(serde::Deserialize)]
        struct Minted {
            token: String,
            expires_at: String,
        }
        let path = format!("/app/installations/{installation}/access_tokens");
        // Boxed: minting is a request, and requests mint tokens.
        let raw = Box::pin(self.execute(Who::App, Method::POST, &path, None)).await?;
        let minted: Minted = decode(&raw.body)?;
        let expires_at = parse_timestamp(&minted.expires_at)
            .ok_or_else(|| Error::Decode(format!("token expires_at {:?}", minted.expires_at)))?;
        let token = InstallationToken {
            token: minted.token,
            expires_at,
        };
        self.auth.store_token(installation, token.clone());
        Ok(token)
    }

    // ── repositories ──

    /// Repositories an installation can access: what a user picks from to connect.
    pub async fn installation_repositories(&self, installation: u64) -> Result<Vec<Repository>> {
        self.list(
            Who::Installation(installation),
            "/installation/repositories?per_page=100",
            Some("repositories"),
        )
        .await
    }

    pub async fn repository(
        &self,
        installation: u64,
        owner: &str,
        repo: &str,
    ) -> Result<Repository> {
        self.get(Who::Installation(installation), &repo_path(owner, repo, ""))
            .await
    }

    pub async fn branch(
        &self,
        installation: u64,
        owner: &str,
        repo: &str,
        branch: &str,
    ) -> Result<Branch> {
        self.get(
            Who::Installation(installation),
            &repo_path(owner, repo, &format!("/branches/{}", encode_ref(branch))),
        )
        .await
    }

    /// Whether roll can push to `branch`, and whether only roll can.
    pub async fn branch_protection(
        &self,
        installation: u64,
        owner: &str,
        repo: &str,
        branch: &str,
    ) -> Result<BranchProtection> {
        let b = self.branch(installation, owner, repo, branch).await?;
        if !b.protected {
            return Ok(BranchProtection {
                roll_can_push: true,
                allows_force_pushes: true,
                ..Default::default()
            });
        }
        let path = repo_path(
            owner,
            repo,
            &format!("/branches/{}/protection", encode_ref(branch)),
        );
        let p: Value = match self.get(Who::Installation(installation), &path).await {
            Ok(p) => p,
            // Without `administration: read` all we know is that it's protected.
            Err(e) if matches!(e.status(), Some(403 | 404)) => {
                return Ok(BranchProtection {
                    protected: true,
                    ..Default::default()
                });
            }
            Err(e) => return Err(e),
        };
        let allows_force_pushes = p["allow_force_pushes"]["enabled"]
            .as_bool()
            .unwrap_or(false);
        let r = &p["restrictions"];
        if r.is_null() {
            return Ok(BranchProtection {
                protected: true,
                details: true,
                roll_can_push: true,
                allows_force_pushes,
                ..Default::default()
            });
        }
        let names = |key: &str, field: &str| -> Vec<String> {
            r[key]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x[field].as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default()
        };
        let apps = names("apps", "slug");
        let others = names("users", "login").len() + names("teams", "slug").len();
        let roll_can_push = apps.contains(&self.config.slug);
        Ok(BranchProtection {
            protected: true,
            details: true,
            restricts_pushes: true,
            only_roll_pushes: roll_can_push && apps.len() == 1 && others == 0,
            roll_can_push,
            allows_force_pushes,
        })
    }

    /// HTTPS git credentials for fetching (import) and pushing (mirror).
    pub async fn git_credentials(
        &self,
        installation: u64,
        owner: &str,
        repo: &str,
    ) -> Result<GitCredentials> {
        let token = self.installation_token(installation).await?;
        Ok(GitCredentials {
            url: format!(
                "{}/{}/{}.git",
                self.config.web_base,
                percent_encode(owner),
                percent_encode(repo)
            ),
            username: "x-access-token".into(),
            password: token.token,
            expires_at: token.expires_at,
        })
    }

    // ── issues ──

    /// Every issue (not pull requests), open and closed. `since` (RFC 3339) limits it to issues
    /// updated since then, for incremental sync.
    pub async fn list_issues(
        &self,
        installation: u64,
        owner: &str,
        repo: &str,
        since: Option<&str>,
    ) -> Result<Vec<Issue>> {
        let mut path = repo_path(
            owner,
            repo,
            "/issues?state=all&per_page=100&sort=updated&direction=asc",
        );
        if let Some(since) = since {
            path.push_str("&since=");
            path.push_str(&percent_encode(since));
        }
        let all: Vec<Issue> = self
            .list(Who::Installation(installation), &path, None)
            .await?;
        Ok(all.into_iter().filter(|i| !i.is_pull_request()).collect())
    }

    pub async fn issue(
        &self,
        installation: u64,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<Issue> {
        self.get(
            Who::Installation(installation),
            &repo_path(owner, repo, &format!("/issues/{number}")),
        )
        .await
    }

    /// Comment on an issue or a pull request.
    pub async fn create_comment(
        &self,
        installation: u64,
        owner: &str,
        repo: &str,
        number: u64,
        body: &str,
    ) -> Result<Comment> {
        let path = repo_path(owner, repo, &format!("/issues/{number}/comments"));
        self.send_json(
            Who::Installation(installation),
            Method::POST,
            &path,
            &json!({ "body": body }),
        )
        .await
    }

    /// Close an issue; `reason` is `completed` or `not_planned`.
    pub async fn close_issue(
        &self,
        installation: u64,
        owner: &str,
        repo: &str,
        number: u64,
        reason: Option<&str>,
    ) -> Result<Issue> {
        let mut body = json!({ "state": "closed" });
        if let Some(reason) = reason {
            body["state_reason"] = reason.into();
        }
        let path = repo_path(owner, repo, &format!("/issues/{number}"));
        self.send_json(Who::Installation(installation), Method::PATCH, &path, &body)
            .await
    }

    /// Replace an issue's labels.
    pub async fn set_labels(
        &self,
        installation: u64,
        owner: &str,
        repo: &str,
        number: u64,
        labels: &[String],
    ) -> Result<Vec<Label>> {
        let path = repo_path(owner, repo, &format!("/issues/{number}/labels"));
        self.send_json(
            Who::Installation(installation),
            Method::PUT,
            &path,
            &json!({ "labels": labels }),
        )
        .await
    }

    // ── pull requests ──

    pub async fn pull_request(
        &self,
        installation: u64,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<PullRequest> {
        self.get(
            Who::Installation(installation),
            &repo_path(owner, repo, &format!("/pulls/{number}")),
        )
        .await
    }

    /// A user's permission on a repository: `admin`, `maintain`, `write`, `triage`, `read` or
    /// `none` (GitHub's collaborator permission API; needs the Metadata permission).
    pub async fn collaborator_permission(
        &self,
        installation: u64,
        owner: &str,
        repo: &str,
        user: &str,
    ) -> Result<String> {
        let v: Value = self
            .get(
                Who::Installation(installation),
                &repo_path(owner, repo, &format!("/collaborators/{}/permission", percent_encode(user))),
            )
            .await?;
        Ok(v.get("permission").and_then(Value::as_str).unwrap_or("none").to_string())
    }

    /// `state`: `open`, `closed` or `all`.
    pub async fn list_pull_requests(
        &self,
        installation: u64,
        owner: &str,
        repo: &str,
        state: &str,
    ) -> Result<Vec<PullRequest>> {
        let path = repo_path(
            owner,
            repo,
            &format!("/pulls?state={}&per_page=100", percent_encode(state)),
        );
        self.list(Who::Installation(installation), &path, None)
            .await
    }

    pub async fn close_pull_request(
        &self,
        installation: u64,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<PullRequest> {
        let path = repo_path(owner, repo, &format!("/pulls/{number}"));
        self.send_json(
            Who::Installation(installation),
            Method::PATCH,
            &path,
            &json!({ "state": "closed" }),
        )
        .await
    }

    // ── checks ──

    pub async fn create_check_run(
        &self,
        installation: u64,
        owner: &str,
        repo: &str,
        run: &CheckRunRequest,
    ) -> Result<CheckRun> {
        let body = serde_json::to_value(run).map_err(|e| Error::Decode(e.to_string()))?;
        self.send_json(
            Who::Installation(installation),
            Method::POST,
            &repo_path(owner, repo, "/check-runs"),
            &body,
        )
        .await
    }

    pub async fn update_check_run(
        &self,
        installation: u64,
        owner: &str,
        repo: &str,
        id: u64,
        run: &CheckRunRequest,
    ) -> Result<CheckRun> {
        let body = serde_json::to_value(run).map_err(|e| Error::Decode(e.to_string()))?;
        let path = repo_path(owner, repo, &format!("/check-runs/{id}"));
        self.send_json(Who::Installation(installation), Method::PATCH, &path, &body)
            .await
    }

    /// Submit a review (with inline comments) on a pull request.
    pub async fn create_review(
        &self,
        installation: u64,
        owner: &str,
        repo: &str,
        number: u64,
        review: &crate::model::CreateReview,
    ) -> Result<crate::model::Review> {
        let path = repo_path(owner, repo, &format!("/pulls/{number}/reviews"));
        let body = serde_json::to_value(review).map_err(|e| Error::Decode(e.to_string()))?;
        self.send_json(Who::Installation(installation), Method::POST, &path, &body).await
    }

    /// The inline comments of one review.
    pub async fn review_comments(
        &self,
        installation: u64,
        owner: &str,
        repo: &str,
        number: u64,
        review_id: u64,
    ) -> Result<Vec<crate::model::ReviewComment>> {
        let path = repo_path(owner, repo, &format!("/pulls/{number}/reviews/{review_id}/comments?per_page=100"));
        self.list(Who::Installation(installation), &path, None).await
    }

    /// Start a review thread: one inline comment on `commit_id`'s version of the diff.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_review_comment(
        &self,
        installation: u64,
        owner: &str,
        repo: &str,
        number: u64,
        commit_id: &str,
        comment: &crate::model::DraftReviewComment,
    ) -> Result<crate::model::ReviewComment> {
        let path = repo_path(owner, repo, &format!("/pulls/{number}/comments"));
        let mut body = serde_json::to_value(comment).map_err(|e| Error::Decode(e.to_string()))?;
        body["commit_id"] = json!(commit_id);
        self.send_json(Who::Installation(installation), Method::POST, &path, &body).await
    }

    /// Reply in the thread that `comment_id` started.
    pub async fn reply_review_comment(
        &self,
        installation: u64,
        owner: &str,
        repo: &str,
        number: u64,
        comment_id: u64,
        body: &str,
    ) -> Result<crate::model::ReviewComment> {
        let path = repo_path(owner, repo, &format!("/pulls/{number}/comments/{comment_id}/replies"));
        self.send_json(Who::Installation(installation), Method::POST, &path, &json!({ "body": body })).await
    }

    // ── GraphQL ──

    /// A GraphQL request as the installation: its `data`, or the errors GitHub reported.
    pub async fn graphql(&self, installation: u64, query: &str, variables: Value) -> Result<Value> {
        let url = graphql_url(&self.config.api_base);
        let body = json!({ "query": query, "variables": variables });
        let raw = self.execute(Who::Installation(installation), Method::POST, &url, Some(&body)).await?;
        let mut v: Value = decode(&raw.body)?;
        if let Some(errors) = v.get("errors").and_then(Value::as_array).filter(|e| !e.is_empty()) {
            let message = errors.iter().filter_map(|e| e.get("message").and_then(Value::as_str)).collect::<Vec<_>>().join("; ");
            return Err(Error::Status { status: 422, message: format!("graphql: {message}") });
        }
        Ok(v["data"].take())
    }

    /// The GraphQL node id of the review thread whose first comment is `root_comment` (a REST
    /// comment id) on pull request `number`, if there is one. Pages through the PR's threads.
    pub async fn review_thread_node(
        &self,
        installation: u64,
        owner: &str,
        repo: &str,
        number: u64,
        root_comment: u64,
    ) -> Result<Option<String>> {
        const Q: &str = "query($owner: String!, $repo: String!, $number: Int!, $after: String) { \
            repository(owner: $owner, name: $repo) { pullRequest(number: $number) { \
            reviewThreads(first: 100, after: $after) { pageInfo { hasNextPage endCursor } \
            nodes { id comments(first: 1) { nodes { databaseId } } } } } } }";
        let mut after: Option<String> = None;
        for _ in 0..self.opts.max_pages {
            let data = self
                .graphql(installation, Q, json!({ "owner": owner, "repo": repo, "number": number, "after": after }))
                .await?;
            let threads = &data["repository"]["pullRequest"]["reviewThreads"];
            for t in threads["nodes"].as_array().into_iter().flatten() {
                if t["comments"]["nodes"][0]["databaseId"].as_u64() == Some(root_comment) {
                    return Ok(t["id"].as_str().map(str::to_string));
                }
            }
            if threads["pageInfo"]["hasNextPage"].as_bool() != Some(true) {
                return Ok(None);
            }
            after = threads["pageInfo"]["endCursor"].as_str().map(str::to_string);
        }
        Ok(None)
    }

    /// Resolve (or unresolve) a review thread by its node id; whether it's resolved now.
    pub async fn set_review_thread_resolved(&self, installation: u64, thread: &str, resolved: bool) -> Result<bool> {
        let (q, field) = if resolved {
            ("mutation($id: ID!) { resolveReviewThread(input: { threadId: $id }) { thread { id isResolved } } }", "resolveReviewThread")
        } else {
            ("mutation($id: ID!) { unresolveReviewThread(input: { threadId: $id }) { thread { id isResolved } } }", "unresolveReviewThread")
        };
        let data = self.graphql(installation, q, json!({ "id": thread })).await?;
        data[field]["thread"]["isResolved"]
            .as_bool()
            .ok_or_else(|| Error::Decode(format!("{field}: no thread in the response")))
    }

    // ── transport ──

    async fn get<T: DeserializeOwned>(&self, who: Who, path: &str) -> Result<T> {
        let raw = self.execute(who, Method::GET, path, None).await?;
        decode(&raw.body)
    }

    async fn send_json<T: DeserializeOwned>(
        &self,
        who: Who,
        method: Method,
        path: &str,
        body: &Value,
    ) -> Result<T> {
        let raw = self.execute(who, method, path, Some(body)).await?;
        decode(&raw.body)
    }

    /// Every page of a list. `key`: the array's field when pages are objects
    /// (`{"total_count": n, "repositories": [...]}`).
    async fn list<T: DeserializeOwned>(
        &self,
        who: Who,
        path: &str,
        key: Option<&str>,
    ) -> Result<Vec<T>> {
        let mut out = Vec::new();
        let mut next = Some(path.to_owned());
        let mut pages = 0;
        while let Some(url) = next.take() {
            pages += 1;
            if pages > self.opts.max_pages {
                warn!(
                    path,
                    pages = self.opts.max_pages,
                    "github list truncated at max_pages"
                );
                break;
            }
            let raw = self.execute(who, Method::GET, &url, None).await?;
            let page: Vec<T> = match key {
                None => decode(&raw.body)?,
                Some(key) => {
                    let mut v: Value = decode(&raw.body)?;
                    serde_json::from_value(v[key].take())
                        .map_err(|e| Error::Decode(format!("{key}: {e}")))?
                }
            };
            out.extend(page);
            next = raw.next;
        }
        Ok(out)
    }

    async fn execute(
        &self,
        who: Who,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Raw> {
        let url = if path.starts_with("http://") || path.starts_with("https://") {
            path.to_owned()
        } else {
            format!("{}{}", self.config.api_base, path)
        };
        let mut attempt = 0;
        loop {
            self.wait_for_budget(who).await?;
            let token = match who {
                Who::App => self.auth.jwt()?,
                Who::Installation(id) => self.installation_token(id).await?.token,
            };
            let mut req = self
                .http
                .request(method.clone(), &url)
                .header(ACCEPT, "application/vnd.github+json")
                .header("x-github-api-version", API_VERSION)
                .header(USER_AGENT, &self.opts.user_agent)
                .header(AUTHORIZATION, format!("Bearer {token}"));
            if let Some(body) = body {
                req = req.json(body);
            }
            let res = req.send().await?;
            let status = res.status();
            let headers = res.headers().clone();
            self.record(who, &headers);
            let bytes = res.bytes().await?;
            if status.is_success() {
                return Ok(Raw {
                    next: next_link(&headers),
                    body: bytes,
                });
            }
            let message = error_message(&bytes);
            let retry = attempt < self.opts.max_retries;
            attempt += 1;
            match status {
                StatusCode::UNAUTHORIZED if retry => {
                    if let Who::Installation(id) = who {
                        // Revoked or expired early: mint a new one.
                        self.auth.forget_token(id);
                        continue;
                    }
                }
                StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS => {
                    if let Some(wait) = self.rate_limit_wait(&headers, &message, status) {
                        let reset = unix_now() + wait.as_secs();
                        if !retry || wait > self.opts.max_backoff {
                            return Err(Error::RateLimited { reset });
                        }
                        debug!(?wait, %status, "github rate limited; waiting");
                        tokio::time::sleep(wait).await;
                        continue;
                    }
                }
                s if s.is_server_error() && retry => {
                    let wait = self
                        .opts
                        .server_error_backoff
                        .saturating_mul(1 << (attempt - 1))
                        .min(self.opts.max_backoff);
                    debug!(?wait, %status, "github server error; retrying");
                    tokio::time::sleep(wait).await;
                    continue;
                }
                _ => {}
            }
            return Err(Error::Status {
                status: status.as_u16(),
                message,
            });
        }
    }

    /// How long a 403/429 asks us to wait, or `None` if it isn't a rate limit.
    fn rate_limit_wait(
        &self,
        headers: &HeaderMap,
        message: &str,
        status: StatusCode,
    ) -> Option<Duration> {
        if let Some(s) = header_u64(headers, RETRY_AFTER.as_str()) {
            return Some(Duration::from_secs(s));
        }
        if header_u64(headers, "x-ratelimit-remaining") == Some(0) {
            let reset = header_u64(headers, "x-ratelimit-reset").unwrap_or(0);
            return Some(Duration::from_secs(reset.saturating_sub(unix_now()).max(1)));
        }
        let lower = message.to_ascii_lowercase();
        if status == StatusCode::TOO_MANY_REQUESTS
            || lower.contains("secondary rate limit")
            || lower.contains("abuse")
        {
            return Some(self.opts.secondary_backoff);
        }
        None
    }

    /// Don't spend a request we know will be refused: wait out a short reset, or fail fast.
    async fn wait_for_budget(&self, who: Who) -> Result<()> {
        let Some(rate) = self.rates.lock().get(&who).copied() else {
            return Ok(());
        };
        let (Some(0), Some(reset)) = (rate.remaining, rate.reset) else {
            return Ok(());
        };
        let now = unix_now();
        if reset <= now {
            return Ok(());
        }
        let wait = Duration::from_secs(reset - now);
        if wait > self.opts.max_backoff {
            return Err(Error::RateLimited { reset });
        }
        tokio::time::sleep(wait).await;
        Ok(())
    }

    fn record(&self, who: Who, headers: &HeaderMap) {
        let remaining = header_u64(headers, "x-ratelimit-remaining");
        if remaining.is_none() {
            return;
        }
        let rate = RateLimit {
            limit: header_u64(headers, "x-ratelimit-limit"),
            remaining,
            reset: header_u64(headers, "x-ratelimit-reset"),
        };
        self.rates.lock().insert(who, rate);
    }
}

fn decode<T: DeserializeOwned>(body: &[u8]) -> Result<T> {
    serde_json::from_slice(body).map_err(|e| Error::Decode(e.to_string()))
}

fn error_message(body: &[u8]) -> String {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| v["message"].as_str().map(str::to_owned))
        .unwrap_or_else(|| String::from_utf8_lossy(&body[..body.len().min(500)]).into_owned())
}

fn header_u64(headers: &HeaderMap, name: &str) -> Option<u64> {
    headers.get(name)?.to_str().ok()?.trim().parse().ok()
}

fn repo_path(owner: &str, repo: &str, rest: &str) -> String {
    format!(
        "/repos/{}/{}{rest}",
        percent_encode(owner),
        percent_encode(repo)
    )
}

/// Encode a branch name for a URL path, keeping its `/`s.
fn encode_ref(name: &str) -> String {
    name.split('/')
        .map(percent_encode)
        .collect::<Vec<_>>()
        .join("/")
}

/// The `rel="next"` URL of a `Link` header.
pub fn next_link(headers: &HeaderMap) -> Option<String> {
    let link = headers.get(LINK)?.to_str().ok()?;
    link.split(',').find_map(|part| {
        let (url, params) = part.split_once(';')?;
        params
            .split(';')
            .any(|p| p.trim() == r#"rel="next""#)
            .then(|| {
                url.trim()
                    .trim_start_matches('<')
                    .trim_end_matches('>')
                    .to_owned()
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;

    #[test]
    fn link_header() {
        let mut h = HeaderMap::new();
        h.insert(
            LINK,
            HeaderValue::from_static(
                r#"<https://api.github.com/repositories/1/issues?page=2>; rel="next", <https://api.github.com/repositories/1/issues?page=5>; rel="last""#,
            ),
        );
        assert_eq!(
            next_link(&h).as_deref(),
            Some("https://api.github.com/repositories/1/issues?page=2")
        );
        h.insert(
            LINK,
            HeaderValue::from_static(r#"<https://x/?page=1>; rel="prev""#),
        );
        assert_eq!(next_link(&h), None);
    }

    #[test]
    fn ref_encoding() {
        assert_eq!(encode_ref("feature/a b"), "feature/a%20b");
    }
}

/// GitHub's GraphQL endpoint for a REST base: `https://api.github.com` → `…/graphql`; GitHub
/// Enterprise Server's `https://host/api/v3` → `https://host/api/graphql`.
pub fn graphql_url(api_base: &str) -> String {
    let base = api_base.trim_end_matches('/');
    match base.strip_suffix("/v3") {
        Some(api) if api.ends_with("/api") => format!("{api}/graphql"),
        _ => format!("{base}/graphql"),
    }
}

#[cfg(test)]
mod graphql_tests {
    use super::graphql_url;

    #[test]
    fn graphql_endpoints() {
        assert_eq!(graphql_url("https://api.github.com"), "https://api.github.com/graphql");
        assert_eq!(graphql_url("https://api.github.com/"), "https://api.github.com/graphql");
        assert_eq!(graphql_url("https://ghe.example.com/api/v3"), "https://ghe.example.com/api/graphql");
        assert_eq!(graphql_url("http://127.0.0.1:9/api"), "http://127.0.0.1:9/api/graphql");
    }
}
