use std::str::FromStr;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use async_trait::async_trait;
use futures::AsyncReadExt;
use gpui::SharedString;
use http_client::{AsyncBody, HttpClient, HttpRequestExt, Request};
use regex::Regex;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use url::Url;
use urlencoding::encode;

use git::{
    BuildCommitPermalinkParams, BuildPermalinkParams, CheckRun, CheckStatus, GitHostingProvider,
    HostingProviderUnauthorized, OAuthAccessTokenPoll, OAuthDeviceAuthorization, ParsedGitRemote,
    PullRequest, PullRequestDetails, PullRequestState, RemoteUrl,
};

use crate::get_host_from_git_remote_url;

fn pull_request_number_regex() -> &'static Regex {
    static PULL_REQUEST_NUMBER_REGEX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\(#(\d+)\)$").unwrap());
    &PULL_REQUEST_NUMBER_REGEX
}

#[derive(Debug, Deserialize)]
struct CommitDetails {
    #[expect(
        unused,
        reason = "This field was found to be unused with serde library bump; it's left as is due to insufficient context on PO's side, but it *may* be fine to remove"
    )]
    commit: Commit,
    author: Option<User>,
}

#[derive(Debug, Deserialize)]
struct Commit {
    #[expect(
        unused,
        reason = "This field was found to be unused with serde library bump; it's left as is due to insufficient context on PO's side, but it *may* be fine to remove"
    )]
    author: Author,
}

#[derive(Debug, Deserialize)]
struct Author {
    #[expect(
        unused,
        reason = "This field was found to be unused with serde library bump; it's left as is due to insufficient context on PO's side, but it *may* be fine to remove"
    )]
    email: String,
}

#[derive(Debug, Deserialize)]
struct User {
    #[expect(
        unused,
        reason = "This field was found to be unused with serde library bump; it's left as is due to insufficient context on PO's side, but it *may* be fine to remove"
    )]
    pub id: u64,
    pub avatar_url: String,
}

// `repo` is the narrowest OAuth scope that grants read access to private
// repositories' pull requests and checks.
const OAUTH_SCOPE: &str = "repo";
const OAUTH_DEVICE_CODE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";

#[derive(Debug, Deserialize)]
struct OAuthDeviceCodeResponse {
    device_code: Option<String>,
    user_code: Option<String>,
    verification_uri: Option<String>,
    interval: Option<u64>,
    error: Option<String>,
    error_description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OAuthAccessTokenResponse {
    access_token: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GithubPullRequest {
    number: u32,
    title: String,
    body: Option<String>,
    state: String,
    #[serde(default)]
    draft: bool,
    merged_at: Option<String>,
    user: Option<GithubUser>,
    html_url: String,
    head: GithubPullRequestRef,
    base: GithubPullRequestRef,
}

#[derive(Debug, Deserialize)]
struct GithubUser {
    login: String,
}

#[derive(Debug, Deserialize)]
struct GithubPullRequestRef {
    #[serde(rename = "ref")]
    name: String,
    sha: String,
}

#[derive(Debug, Deserialize)]
struct GithubCheckRuns {
    check_runs: Vec<GithubCheckRun>,
}

#[derive(Debug, Deserialize)]
struct GithubCheckRun {
    name: String,
    status: String,
    conclusion: Option<String>,
    html_url: Option<String>,
    details_url: Option<String>,
    output: Option<GithubCheckRunOutput>,
}

#[derive(Debug, Deserialize)]
struct GithubCheckRunOutput {
    title: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GithubCombinedStatus {
    statuses: Vec<GithubCommitStatus>,
}

#[derive(Debug, Deserialize)]
struct GithubCommitStatus {
    context: String,
    state: String,
    description: Option<String>,
    target_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GithubErrorResponse {
    message: String,
}

impl TryFrom<GithubPullRequest> for PullRequestDetails {
    type Error = anyhow::Error;

    fn try_from(pull_request: GithubPullRequest) -> Result<Self> {
        let state = if pull_request.merged_at.is_some() {
            PullRequestState::Merged
        } else if pull_request.state == "closed" {
            PullRequestState::Closed
        } else if pull_request.draft {
            PullRequestState::Draft
        } else {
            PullRequestState::Open
        };

        Ok(Self {
            number: pull_request.number,
            title: pull_request.title.into(),
            body: pull_request.body.unwrap_or_default().into(),
            state,
            author: pull_request.user.map(|user| user.login.into()),
            url: Url::parse(&pull_request.html_url).context("invalid pull request URL")?,
            head_branch: pull_request.head.name.into(),
            head_sha: pull_request.head.sha.into(),
            base_branch: pull_request.base.name.into(),
        })
    }
}

impl From<GithubCheckRun> for CheckRun {
    fn from(check_run: GithubCheckRun) -> Self {
        let status = if check_run.status != "completed" {
            CheckStatus::Pending
        } else {
            match check_run.conclusion.as_deref() {
                Some("success") => CheckStatus::Success,
                Some("failure" | "timed_out" | "action_required" | "startup_failure") => {
                    CheckStatus::Failure
                }
                Some("cancelled") => CheckStatus::Cancelled,
                Some("skipped") => CheckStatus::Skipped,
                _ => CheckStatus::Neutral,
            }
        };

        Self {
            name: check_run.name.into(),
            status,
            description: check_run
                .output
                .and_then(|output| output.title)
                .map(Into::into),
            url: check_run
                .html_url
                .or(check_run.details_url)
                .and_then(|url| Url::parse(&url).ok()),
        }
    }
}

impl From<GithubCommitStatus> for CheckRun {
    fn from(commit_status: GithubCommitStatus) -> Self {
        let status = match commit_status.state.as_str() {
            "success" => CheckStatus::Success,
            "failure" | "error" => CheckStatus::Failure,
            _ => CheckStatus::Pending,
        };

        Self {
            name: commit_status.context.into(),
            status,
            description: commit_status.description.map(Into::into),
            url: commit_status
                .target_url
                .and_then(|url| Url::parse(&url).ok()),
        }
    }
}

fn select_pull_request(pull_requests: Vec<GithubPullRequest>) -> Option<GithubPullRequest> {
    let open_index = pull_requests
        .iter()
        .position(|pull_request| pull_request.state == "open");
    pull_requests.into_iter().nth(open_index.unwrap_or(0))
}

fn oauth_error(error: &str, description: Option<String>) -> anyhow::Error {
    match error {
        "expired_token" => anyhow::anyhow!("The sign-in code expired. Please try again."),
        "access_denied" => anyhow::anyhow!("Sign-in was cancelled."),
        _ => anyhow::anyhow!(
            "Sign-in failed: {}",
            description.unwrap_or_else(|| error.to_string())
        ),
    }
}

#[derive(Debug)]
pub struct Github {
    name: String,
    base_url: Url,
}

fn normalize_author_email(email: &str) -> &str {
    email.trim_start_matches('<').trim_end_matches('>')
}

fn build_cdn_avatar_url(email: &str) -> Result<Url> {
    let email = normalize_author_email(email);
    Url::parse(&format!(
        "https://avatars.githubusercontent.com/u/e?email={}&s=128",
        encode(email)
    ))
    .context("failed to construct avatar URL")
}

fn build_cdn_avatar_url_for_author_email(email: &str) -> Result<Option<Url>> {
    let email = normalize_author_email(email);
    if email.ends_with("[bot]@users.noreply.github.com") {
        return Ok(None);
    }

    build_cdn_avatar_url(email).map(Some)
}

impl Github {
    pub fn new(name: impl Into<String>, base_url: Url) -> Self {
        Self {
            name: name.into(),
            base_url,
        }
    }

    pub fn public_instance() -> Self {
        Self::new("GitHub", Url::parse("https://github.com").unwrap())
    }

    pub fn from_remote_url(remote_url: &str) -> Result<Self> {
        let host = get_host_from_git_remote_url(remote_url)?;
        if host == "github.com" {
            bail!("the GitHub instance is not self-hosted");
        }

        // TODO: detecting self hosted instances by checking whether "github" is in the url or not
        // is not very reliable. See https://github.com/zed-industries/zed/issues/26393 for more
        // information.
        if !host.contains("github") {
            bail!("not a GitHub URL");
        }

        Ok(Self::new(
            "GitHub Self-Hosted",
            Url::parse(&format!("https://{}", host))?,
        ))
    }

    fn api_base_url(&self) -> Result<String> {
        let Some(host) = self.base_url.host_str() else {
            bail!("failed to get host from github base url");
        };
        Ok(if host == "github.com" {
            "https://api.github.com".to_string()
        } else if host.ends_with(".ghe.com") {
            format!("https://api.{host}")
        } else {
            format!("https://{host}/api/v3")
        })
    }

    async fn send_api_request<T: DeserializeOwned>(
        &self,
        path_and_query: &str,
        access_token: Option<&str>,
        http_client: &Arc<dyn HttpClient>,
    ) -> Result<T> {
        let url = format!("{}{path_and_query}", self.api_base_url()?);
        let mut request = Request::get(&url)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .follow_redirects(http_client::RedirectPolicy::FollowAll);

        let environment_token = std::env::var("GITHUB_TOKEN").ok();
        let access_token = access_token.or(environment_token.as_deref());
        if let Some(access_token) = access_token {
            request = request.header("Authorization", format!("Bearer {access_token}"));
        }

        let mut response = http_client
            .send(request.body(AsyncBody::default())?)
            .await
            .with_context(|| format!("error sending GitHub request to {url}"))?;
        let mut body = Vec::new();
        response.body_mut().read_to_end(&mut body).await?;

        let status = response.status();
        if !status.is_success() {
            let message = serde_json::from_slice::<GithubErrorResponse>(&body)
                .map(|error| error.message)
                .unwrap_or_else(|_| String::from_utf8_lossy(&body).into_owned());
            // Unauthenticated requests get a 404 for private repositories and a
            // 403 once the anonymous rate limit is exhausted; signing in fixes both.
            if status.as_u16() == 401
                || (access_token.is_none() && matches!(status.as_u16(), 403 | 404))
            {
                return Err(HostingProviderUnauthorized {
                    message: format!("Sign in to {} to continue ({message})", self.name),
                }
                .into());
            }
            bail!("GitHub request failed with status {status}: {message}");
        }

        serde_json::from_slice(&body)
            .with_context(|| format!("failed to deserialize GitHub response from {url}"))
    }

    async fn send_oauth_request<T: DeserializeOwned>(
        &self,
        path: &str,
        form: &[(&str, &str)],
        http_client: &Arc<dyn HttpClient>,
    ) -> Result<T> {
        let url = self.base_url.join(path)?;
        let body = form
            .iter()
            .map(|(key, value)| format!("{}={}", encode(key), encode(value)))
            .collect::<Vec<_>>()
            .join("&");
        let request = Request::post(url.as_str())
            .header("Accept", "application/json")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(AsyncBody::from(body))?;

        let mut response = http_client
            .send(request)
            .await
            .with_context(|| format!("error sending GitHub OAuth request to {url}"))?;
        let mut body = Vec::new();
        response.body_mut().read_to_end(&mut body).await?;

        // GitHub reports OAuth errors in the JSON body, sometimes with a
        // success status, so the body is parsed regardless of the status.
        serde_json::from_slice(&body).with_context(|| {
            format!(
                "failed to deserialize GitHub OAuth response ({}): {}",
                response.status(),
                String::from_utf8_lossy(&body)
            )
        })
    }

    async fn fetch_github_commit_author(
        &self,
        repo_owner: &str,
        repo: &str,
        commit: &str,
        client: &Arc<dyn HttpClient>,
    ) -> Result<Option<User>> {
        let Some(host) = self.base_url.host_str() else {
            bail!("failed to get host from github base url");
        };
        let url = format!("https://api.{host}/repos/{repo_owner}/{repo}/commits/{commit}");

        let mut request = Request::get(&url)
            .header("Content-Type", "application/json")
            .follow_redirects(http_client::RedirectPolicy::FollowAll);

        if let Ok(github_token) = std::env::var("GITHUB_TOKEN") {
            request = request.header("Authorization", format!("Bearer {}", github_token));
        }

        let mut response = client
            .send(request.body(AsyncBody::default())?)
            .await
            .with_context(|| format!("error fetching GitHub commit details at {:?}", url))?;

        let mut body = Vec::new();
        response.body_mut().read_to_end(&mut body).await?;

        if response.status().is_client_error() {
            let text = String::from_utf8_lossy(body.as_slice());
            bail!(
                "status error {}, response: {text:?}",
                response.status().as_u16()
            );
        }

        let body_str = std::str::from_utf8(&body)?;

        serde_json::from_str::<CommitDetails>(body_str)
            .map(|commit| commit.author)
            .context("failed to deserialize GitHub commit details")
    }
}

#[async_trait]
impl GitHostingProvider for Github {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn base_url(&self) -> Url {
        self.base_url.clone()
    }

    fn supports_avatars(&self) -> bool {
        // Avatars are not supported for self-hosted GitHub instances
        // See tracking issue: https://github.com/zed-industries/zed/issues/11043
        &self.name == "GitHub"
    }

    fn format_line_number(&self, line: u32) -> String {
        format!("L{line}")
    }

    fn format_line_numbers(&self, start_line: u32, end_line: u32) -> String {
        format!("L{start_line}-L{end_line}")
    }

    fn parse_remote_url(&self, url: &str) -> Option<ParsedGitRemote> {
        let url = RemoteUrl::from_str(url).ok()?;

        let host = url.host_str()?;
        if host != self.base_url.host_str()? {
            return None;
        }

        let mut path_segments = url.path_segments()?;
        let mut owner = path_segments.next()?;
        if owner.is_empty() {
            owner = path_segments.next()?;
        }

        let repo = path_segments.next()?.trim_end_matches(".git");

        Some(ParsedGitRemote {
            owner: owner.into(),
            repo: repo.into(),
        })
    }

    fn build_commit_permalink(
        &self,
        remote: &ParsedGitRemote,
        params: BuildCommitPermalinkParams,
    ) -> Url {
        let BuildCommitPermalinkParams { sha } = params;
        let ParsedGitRemote { owner, repo } = remote;

        self.base_url()
            .join(&format!("{owner}/{repo}/commit/{sha}"))
            .unwrap()
    }

    fn build_permalink(&self, remote: ParsedGitRemote, params: BuildPermalinkParams) -> Url {
        let ParsedGitRemote { owner, repo } = remote;
        let BuildPermalinkParams {
            sha,
            path,
            selection,
        } = params;

        let mut permalink = self
            .base_url()
            .join(&format!("{owner}/{repo}/blob/{sha}/{path}"))
            .unwrap();
        if path.ends_with(".md") {
            permalink.set_query(Some("plain=1"));
        }
        permalink.set_fragment(
            selection
                .map(|selection| self.line_fragment(&selection))
                .as_deref(),
        );
        permalink
    }

    fn build_create_pull_request_url(
        &self,
        remote: &ParsedGitRemote,
        source_branch: &str,
    ) -> Option<Url> {
        let ParsedGitRemote { owner, repo } = remote;
        let encoded_source = encode(source_branch);

        self.base_url()
            .join(&format!("{owner}/{repo}/pull/new/{encoded_source}"))
            .ok()
    }

    fn extract_pull_request(&self, remote: &ParsedGitRemote, message: &str) -> Option<PullRequest> {
        let line = message.lines().next()?;
        let capture = pull_request_number_regex().captures(line)?;
        let number = capture.get(1)?.as_str().parse::<u32>().ok()?;

        let mut url = self.base_url();
        let path = format!("/{}/{}/pull/{}", remote.owner, remote.repo, number);
        url.set_path(&path);

        Some(PullRequest { number, url })
    }

    async fn commit_author_avatar_url(
        &self,
        repo_owner: &str,
        repo: &str,
        commit: SharedString,
        author_email: Option<SharedString>,
        http_client: Arc<dyn HttpClient>,
    ) -> Result<Option<Url>> {
        if let Some(email) = author_email
            && let Some(avatar_url) = build_cdn_avatar_url_for_author_email(&email)?
        {
            return Ok(Some(avatar_url));
        }

        let commit = commit.to_string();
        let avatar_url = self
            .fetch_github_commit_author(repo_owner, repo, &commit, &http_client)
            .await?
            .map(|author| -> Result<Url, url::ParseError> {
                let mut url = Url::parse(&author.avatar_url)?;
                url.set_query(Some("size=128"));
                Ok(url)
            })
            .transpose()?;
        Ok(avatar_url)
    }

    fn supports_pull_request_details(&self) -> bool {
        true
    }

    async fn request_oauth_device_authorization(
        &self,
        client_id: &str,
        http_client: Arc<dyn HttpClient>,
    ) -> Result<OAuthDeviceAuthorization> {
        let response: OAuthDeviceCodeResponse = self
            .send_oauth_request(
                "login/device/code",
                &[("client_id", client_id), ("scope", OAUTH_SCOPE)],
                &http_client,
            )
            .await?;

        if let Some(error) = response.error {
            return Err(oauth_error(&error, response.error_description));
        }

        let (Some(device_code), Some(user_code), Some(verification_uri)) = (
            response.device_code,
            response.user_code,
            response.verification_uri,
        ) else {
            bail!("GitHub returned an incomplete device authorization response");
        };

        Ok(OAuthDeviceAuthorization {
            user_code: user_code.into(),
            verification_uri: verification_uri.into(),
            device_code,
            poll_interval: Duration::from_secs(response.interval.unwrap_or(5).max(1)),
        })
    }

    async fn poll_oauth_access_token(
        &self,
        client_id: &str,
        authorization: &OAuthDeviceAuthorization,
        http_client: Arc<dyn HttpClient>,
    ) -> Result<OAuthAccessTokenPoll> {
        let response: OAuthAccessTokenResponse = self
            .send_oauth_request(
                "login/oauth/access_token",
                &[
                    ("client_id", client_id),
                    ("device_code", &authorization.device_code),
                    ("grant_type", OAUTH_DEVICE_CODE_GRANT_TYPE),
                ],
                &http_client,
            )
            .await?;

        if let Some(access_token) = response.access_token {
            return Ok(OAuthAccessTokenPoll::Granted(access_token));
        }

        match response.error.as_deref() {
            Some("authorization_pending") => Ok(OAuthAccessTokenPoll::Pending),
            Some("slow_down") => Ok(OAuthAccessTokenPoll::SlowDown),
            Some(error) => Err(oauth_error(error, response.error_description)),
            None => bail!("GitHub returned an unexpected access token response"),
        }
    }

    async fn find_pull_request(
        &self,
        remote: &ParsedGitRemote,
        head_owner: &str,
        head_branch: &str,
        access_token: Option<&str>,
        http_client: Arc<dyn HttpClient>,
    ) -> Result<Option<PullRequestDetails>> {
        let ParsedGitRemote { owner, repo } = remote;
        let head = encode(&format!("{head_owner}:{head_branch}")).into_owned();
        let pull_requests: Vec<GithubPullRequest> = self
            .send_api_request(
                &format!("/repos/{owner}/{repo}/pulls?head={head}&state=all&per_page=10"),
                access_token,
                &http_client,
            )
            .await?;

        select_pull_request(pull_requests)
            .map(PullRequestDetails::try_from)
            .transpose()
    }

    async fn pull_request_checks(
        &self,
        remote: &ParsedGitRemote,
        pull_request: &PullRequestDetails,
        access_token: Option<&str>,
        http_client: Arc<dyn HttpClient>,
    ) -> Result<Vec<CheckRun>> {
        let ParsedGitRemote { owner, repo } = remote;
        let sha = &pull_request.head_sha;
        let check_runs_path =
            format!("/repos/{owner}/{repo}/commits/{sha}/check-runs?per_page=100");
        let statuses_path = format!("/repos/{owner}/{repo}/commits/{sha}/status?per_page=100");

        // GitHub Actions and GitHub Apps report check runs, while many external
        // CI services still report legacy commit statuses.
        let (check_runs, combined_status) = futures::join!(
            self.send_api_request::<GithubCheckRuns>(&check_runs_path, access_token, &http_client),
            self.send_api_request::<GithubCombinedStatus>(
                &statuses_path,
                access_token,
                &http_client
            ),
        );

        let mut checks = check_runs?
            .check_runs
            .into_iter()
            .map(CheckRun::from)
            .chain(combined_status?.statuses.into_iter().map(CheckRun::from))
            .collect::<Vec<_>>();
        checks.sort_by(|left, right| {
            left.status
                .cmp(&right.status)
                .then_with(|| left.name.cmp(&right.name))
        });
        Ok(checks)
    }
}

#[cfg(test)]
mod tests {
    use git::repository::repo_path;
    use indoc::indoc;
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn test_remote_url_with_root_slash() {
        let remote_url = "git@github.com:/zed-industries/zed";
        let parsed_remote = Github::public_instance()
            .parse_remote_url(remote_url)
            .unwrap();

        assert_eq!(
            parsed_remote,
            ParsedGitRemote {
                owner: "zed-industries".into(),
                repo: "zed".into(),
            }
        );
    }

    #[test]
    fn test_invalid_self_hosted_remote_url() {
        let remote_url = "git@github.com:zed-industries/zed.git";
        let github = Github::from_remote_url(remote_url);
        assert!(github.is_err());
    }

    #[test]
    fn test_from_remote_url_ssh() {
        let remote_url = "git@github.my-enterprise.com:zed-industries/zed.git";
        let github = Github::from_remote_url(remote_url).unwrap();

        assert!(!github.supports_avatars());
        assert_eq!(github.name, "GitHub Self-Hosted".to_string());
        assert_eq!(
            github.base_url,
            Url::parse("https://github.my-enterprise.com").unwrap()
        );
    }

    #[test]
    fn test_from_remote_url_https() {
        let remote_url = "https://github.my-enterprise.com/zed-industries/zed.git";
        let github = Github::from_remote_url(remote_url).unwrap();

        assert!(!github.supports_avatars());
        assert_eq!(github.name, "GitHub Self-Hosted".to_string());
        assert_eq!(
            github.base_url,
            Url::parse("https://github.my-enterprise.com").unwrap()
        );
    }

    #[test]
    fn test_parse_remote_url_given_self_hosted_ssh_url() {
        let remote_url = "git@github.my-enterprise.com:zed-industries/zed.git";
        let parsed_remote = Github::from_remote_url(remote_url)
            .unwrap()
            .parse_remote_url(remote_url)
            .unwrap();

        assert_eq!(
            parsed_remote,
            ParsedGitRemote {
                owner: "zed-industries".into(),
                repo: "zed".into(),
            }
        );
    }

    #[test]
    fn test_parse_remote_url_given_self_hosted_https_url_with_subgroup() {
        let remote_url = "https://github.my-enterprise.com/zed-industries/zed.git";
        let parsed_remote = Github::from_remote_url(remote_url)
            .unwrap()
            .parse_remote_url(remote_url)
            .unwrap();

        assert_eq!(
            parsed_remote,
            ParsedGitRemote {
                owner: "zed-industries".into(),
                repo: "zed".into(),
            }
        );
    }

    #[test]
    fn test_parse_remote_url_given_ssh_url() {
        let parsed_remote = Github::public_instance()
            .parse_remote_url("git@github.com:zed-industries/zed.git")
            .unwrap();

        assert_eq!(
            parsed_remote,
            ParsedGitRemote {
                owner: "zed-industries".into(),
                repo: "zed".into(),
            }
        );
    }

    #[test]
    fn test_parse_remote_url_given_https_url() {
        let parsed_remote = Github::public_instance()
            .parse_remote_url("https://github.com/zed-industries/zed.git")
            .unwrap();

        assert_eq!(
            parsed_remote,
            ParsedGitRemote {
                owner: "zed-industries".into(),
                repo: "zed".into(),
            }
        );
    }

    #[test]
    fn test_parse_remote_url_given_https_url_with_username() {
        let parsed_remote = Github::public_instance()
            .parse_remote_url("https://jlannister@github.com/some-org/some-repo.git")
            .unwrap();

        assert_eq!(
            parsed_remote,
            ParsedGitRemote {
                owner: "some-org".into(),
                repo: "some-repo".into(),
            }
        );
    }

    #[test]
    fn test_build_github_permalink_from_ssh_url() {
        let remote = ParsedGitRemote {
            owner: "zed-industries".into(),
            repo: "zed".into(),
        };
        let permalink = Github::public_instance().build_permalink(
            remote,
            BuildPermalinkParams::new(
                "e6ebe7974deb6bb6cc0e2595c8ec31f0c71084b7",
                &repo_path("crates/editor/src/git/permalink.rs"),
                None,
            ),
        );

        let expected_url = "https://github.com/zed-industries/zed/blob/e6ebe7974deb6bb6cc0e2595c8ec31f0c71084b7/crates/editor/src/git/permalink.rs";
        assert_eq!(permalink.to_string(), expected_url.to_string())
    }

    #[test]
    fn test_build_github_permalink() {
        let permalink = Github::public_instance().build_permalink(
            ParsedGitRemote {
                owner: "zed-industries".into(),
                repo: "zed".into(),
            },
            BuildPermalinkParams::new(
                "b2efec9824c45fcc90c9a7eb107a50d1772a60aa",
                &repo_path("crates/zed/src/main.rs"),
                None,
            ),
        );

        let expected_url = "https://github.com/zed-industries/zed/blob/b2efec9824c45fcc90c9a7eb107a50d1772a60aa/crates/zed/src/main.rs";
        assert_eq!(permalink.to_string(), expected_url.to_string())
    }

    #[test]
    fn test_build_github_permalink_with_single_line_selection() {
        let permalink = Github::public_instance().build_permalink(
            ParsedGitRemote {
                owner: "zed-industries".into(),
                repo: "zed".into(),
            },
            BuildPermalinkParams::new(
                "e6ebe7974deb6bb6cc0e2595c8ec31f0c71084b7",
                &repo_path("crates/editor/src/git/permalink.rs"),
                Some(6..6),
            ),
        );

        let expected_url = "https://github.com/zed-industries/zed/blob/e6ebe7974deb6bb6cc0e2595c8ec31f0c71084b7/crates/editor/src/git/permalink.rs#L7";
        assert_eq!(permalink.to_string(), expected_url.to_string())
    }

    #[test]
    fn test_build_github_permalink_with_multi_line_selection() {
        let permalink = Github::public_instance().build_permalink(
            ParsedGitRemote {
                owner: "zed-industries".into(),
                repo: "zed".into(),
            },
            BuildPermalinkParams::new(
                "e6ebe7974deb6bb6cc0e2595c8ec31f0c71084b7",
                &repo_path("crates/editor/src/git/permalink.rs"),
                Some(23..47),
            ),
        );

        let expected_url = "https://github.com/zed-industries/zed/blob/e6ebe7974deb6bb6cc0e2595c8ec31f0c71084b7/crates/editor/src/git/permalink.rs#L24-L48";
        assert_eq!(permalink.to_string(), expected_url.to_string())
    }

    #[test]
    fn test_build_github_create_pr_url() {
        let remote = ParsedGitRemote {
            owner: "zed-industries".into(),
            repo: "zed".into(),
        };

        let provider = Github::public_instance();

        let url = provider
            .build_create_pull_request_url(&remote, "feature/something cool")
            .expect("url should be constructed");

        assert_eq!(
            url.as_str(),
            "https://github.com/zed-industries/zed/pull/new/feature%2Fsomething%20cool"
        );
    }

    #[test]
    fn test_github_pull_requests() {
        let remote = ParsedGitRemote {
            owner: "zed-industries".into(),
            repo: "zed".into(),
        };

        let github = Github::public_instance();
        let message = "This does not contain a pull request";
        assert!(github.extract_pull_request(&remote, message).is_none());

        // Pull request number at end of first line
        let message = indoc! {r#"
            project panel: do not expand collapsed worktrees on "collapse all entries" (#10687)

            Fixes #10597

            Release Notes:

            - Fixed "project panel: collapse all entries" expanding collapsed worktrees.
            "#
        };

        assert_eq!(
            github
                .extract_pull_request(&remote, message)
                .unwrap()
                .url
                .as_str(),
            "https://github.com/zed-industries/zed/pull/10687"
        );

        // Pull request number in middle of line, which we want to ignore
        let message = indoc! {r#"
            Follow-up to #10687 to fix problems

            See the original PR, this is a fix.
            "#
        };
        assert_eq!(github.extract_pull_request(&remote, message), None);
    }

    /// Regression test for issue #39875
    #[test]
    fn test_git_permalink_url_escaping() {
        let permalink = Github::public_instance().build_permalink(
            ParsedGitRemote {
                owner: "zed-industries".into(),
                repo: "nonexistent".into(),
            },
            BuildPermalinkParams::new(
                "3ef1539900037dd3601be7149b2b39ed6d0ce3db",
                &repo_path("app/blog/[slug]/page.tsx"),
                Some(7..7),
            ),
        );

        let expected_url = "https://github.com/zed-industries/nonexistent/blob/3ef1539900037dd3601be7149b2b39ed6d0ce3db/app/blog/%5Bslug%5D/page.tsx#L8";
        assert_eq!(permalink.to_string(), expected_url.to_string())
    }

    #[test]
    fn test_build_create_pull_request_url() {
        let remote = ParsedGitRemote {
            owner: "zed-industries".into(),
            repo: "zed".into(),
        };

        let github = Github::public_instance();
        let url = github
            .build_create_pull_request_url(&remote, "feature/new-feature")
            .unwrap();

        assert_eq!(
            url.as_str(),
            "https://github.com/zed-industries/zed/pull/new/feature%2Fnew-feature"
        );

        let base_url = Url::parse("https://github.zed.com").unwrap();
        let github = Github::new("GitHub Self-Hosted", base_url);
        let url = github
            .build_create_pull_request_url(&remote, "feature/new-feature")
            .expect("should be able to build pull request url");

        assert_eq!(
            url.as_str(),
            "https://github.zed.com/zed-industries/zed/pull/new/feature%2Fnew-feature"
        );
    }

    #[test]
    fn test_build_cdn_avatar_url_simple_email() {
        let url = build_cdn_avatar_url("user@example.com").unwrap();
        assert_eq!(
            url.as_str(),
            "https://avatars.githubusercontent.com/u/e?email=user%40example.com&s=128"
        );
    }

    #[test]
    fn test_build_cdn_avatar_url_with_angle_brackets() {
        let url = build_cdn_avatar_url("<user@example.com>").unwrap();
        assert_eq!(
            url.as_str(),
            "https://avatars.githubusercontent.com/u/e?email=user%40example.com&s=128"
        );
    }

    #[test]
    fn test_build_cdn_avatar_url_with_special_chars() {
        let url = build_cdn_avatar_url("user+tag@example.com").unwrap();
        assert_eq!(
            url.as_str(),
            "https://avatars.githubusercontent.com/u/e?email=user%2Btag%40example.com&s=128"
        );
    }

    #[test]
    fn test_build_cdn_avatar_url_for_author_email_skips_bot_noreply_emails() {
        for email in [
            "41898282+github-actions[bot]@users.noreply.github.com",
            "<41898282+github-actions[bot]@users.noreply.github.com>",
        ] {
            assert_eq!(build_cdn_avatar_url_for_author_email(email).unwrap(), None);
        }
    }

    #[test]
    fn test_build_cdn_avatar_url_for_author_email_uses_user_noreply_emails() {
        let url = build_cdn_avatar_url_for_author_email("12345+octocat@users.noreply.github.com")
            .unwrap()
            .unwrap();

        assert_eq!(
            url.as_str(),
            "https://avatars.githubusercontent.com/u/e?email=12345%2Boctocat%40users.noreply.github.com&s=128"
        );
    }

    fn github_pull_request(state: &str, draft: bool, merged_at: Option<&str>) -> GithubPullRequest {
        serde_json::from_value(serde_json::json!({
            "number": 42,
            "title": "Add feature",
            "body": null,
            "state": state,
            "draft": draft,
            "merged_at": merged_at,
            "user": { "login": "octocat" },
            "html_url": "https://github.com/zed-industries/zed/pull/42",
            "head": { "ref": "feature", "sha": "abc123" },
            "base": { "ref": "main", "sha": "def456" },
        }))
        .unwrap()
    }

    #[test]
    fn test_pull_request_details_state() {
        let cases = [
            (("open", false, None), PullRequestState::Open),
            (("open", true, None), PullRequestState::Draft),
            (("closed", false, None), PullRequestState::Closed),
            (
                ("closed", false, Some("2024-01-01T00:00:00Z")),
                PullRequestState::Merged,
            ),
        ];

        for ((state, draft, merged_at), expected_state) in cases {
            let details =
                PullRequestDetails::try_from(github_pull_request(state, draft, merged_at)).unwrap();
            assert_eq!(details.state, expected_state);
        }

        let details =
            PullRequestDetails::try_from(github_pull_request("open", false, None)).unwrap();
        assert_eq!(details.number, 42);
        assert_eq!(details.body.as_ref(), "");
        assert_eq!(details.author.as_deref(), Some("octocat"));
        assert_eq!(details.head_branch.as_ref(), "feature");
        assert_eq!(details.head_sha.as_ref(), "abc123");
        assert_eq!(details.base_branch.as_ref(), "main");
    }

    #[test]
    fn test_select_pull_request_prefers_open() {
        let mut closed = github_pull_request("closed", false, None);
        closed.number = 1;
        let mut open = github_pull_request("open", false, None);
        open.number = 2;

        let selected = select_pull_request(vec![closed, open]).unwrap();
        assert_eq!(selected.number, 2);

        let mut first_closed = github_pull_request("closed", false, None);
        first_closed.number = 3;
        let mut second_closed = github_pull_request("closed", false, None);
        second_closed.number = 4;
        let selected = select_pull_request(vec![first_closed, second_closed]).unwrap();
        assert_eq!(selected.number, 3);

        assert!(select_pull_request(Vec::new()).is_none());
    }

    #[test]
    fn test_check_run_status() {
        let check_run = |status: &str, conclusion: Option<&str>| -> CheckRun {
            serde_json::from_value::<GithubCheckRun>(serde_json::json!({
                "name": "test",
                "status": status,
                "conclusion": conclusion,
                "html_url": "https://github.com/zed-industries/zed/runs/1",
                "details_url": null,
                "output": { "title": "All good" },
            }))
            .unwrap()
            .into()
        };

        assert_eq!(check_run("in_progress", None).status, CheckStatus::Pending);
        assert_eq!(check_run("queued", None).status, CheckStatus::Pending);
        assert_eq!(
            check_run("completed", Some("success")).status,
            CheckStatus::Success
        );
        assert_eq!(
            check_run("completed", Some("timed_out")).status,
            CheckStatus::Failure
        );
        assert_eq!(
            check_run("completed", Some("cancelled")).status,
            CheckStatus::Cancelled
        );
        assert_eq!(
            check_run("completed", Some("skipped")).status,
            CheckStatus::Skipped
        );
        assert_eq!(
            check_run("completed", Some("neutral")).status,
            CheckStatus::Neutral
        );

        let check = check_run("completed", Some("success"));
        assert_eq!(check.description.as_deref(), Some("All good"));
        assert_eq!(
            check.url.unwrap().as_str(),
            "https://github.com/zed-industries/zed/runs/1"
        );
    }

    #[test]
    fn test_commit_status() {
        let commit_status = |state: &str| -> CheckRun {
            serde_json::from_value::<GithubCommitStatus>(serde_json::json!({
                "context": "ci/external",
                "state": state,
                "description": null,
                "target_url": null,
            }))
            .unwrap()
            .into()
        };

        assert_eq!(commit_status("success").status, CheckStatus::Success);
        assert_eq!(commit_status("failure").status, CheckStatus::Failure);
        assert_eq!(commit_status("error").status, CheckStatus::Failure);
        assert_eq!(commit_status("pending").status, CheckStatus::Pending);
        assert_eq!(commit_status("success").name.as_ref(), "ci/external");
    }

    #[test]
    fn test_api_base_url() {
        assert_eq!(
            Github::public_instance().api_base_url().unwrap(),
            "https://api.github.com"
        );
        assert_eq!(
            Github::new("GitHub", Url::parse("https://acme.ghe.com").unwrap())
                .api_base_url()
                .unwrap(),
            "https://api.acme.ghe.com"
        );
        assert_eq!(
            Github::new("GitHub", Url::parse("https://github.corp.com").unwrap())
                .api_base_url()
                .unwrap(),
            "https://github.corp.com/api/v3"
        );
    }
}
