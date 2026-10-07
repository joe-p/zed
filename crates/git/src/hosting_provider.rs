use std::{ops::Range, sync::Arc, time::Duration};

use anyhow::{Result, bail};
use async_trait::async_trait;
use derive_more::{Deref, DerefMut};
use gpui::{App, Global, SharedString};
use http_client::HttpClient;
use itertools::Itertools;
use parking_lot::RwLock;
use url::Url;

use crate::repository::RepoPath;

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct PullRequest {
    pub number: u32,
    pub url: Url,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullRequestState {
    Open,
    Draft,
    Merged,
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestDetails {
    pub number: u32,
    pub title: SharedString,
    pub body: SharedString,
    pub state: PullRequestState,
    pub author: Option<SharedString>,
    pub url: Url,
    pub head_branch: SharedString,
    pub head_sha: SharedString,
    pub base_branch: SharedString,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CheckStatus {
    Failure,
    Pending,
    Cancelled,
    Success,
    Neutral,
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckRun {
    pub name: SharedString,
    pub status: CheckStatus,
    pub description: Option<SharedString>,
    pub url: Option<Url>,
}

/// An in-progress OAuth device authorization. The user must visit
/// `verification_uri` and enter `user_code` to complete it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthDeviceAuthorization {
    pub user_code: SharedString,
    pub verification_uri: SharedString,
    pub device_code: String,
    pub poll_interval: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OAuthAccessTokenPoll {
    Pending,
    SlowDown,
    Granted(String),
}

/// Returned by hosting provider API calls when the credentials are missing,
/// expired, or revoked, so callers can prompt the user to sign in again.
#[derive(Debug)]
pub struct HostingProviderUnauthorized {
    pub message: String,
}

impl std::fmt::Display for HostingProviderUnauthorized {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for HostingProviderUnauthorized {}

#[derive(Clone)]
pub struct GitRemote {
    pub host: Arc<dyn GitHostingProvider + Send + Sync + 'static>,
    pub owner: SharedString,
    pub repo: SharedString,
}

impl std::fmt::Debug for GitRemote {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitRemote")
            .field("host", &self.host.name())
            .field("owner", &self.owner)
            .field("repo", &self.repo)
            .finish()
    }
}

impl GitRemote {
    pub fn host_supports_avatars(&self) -> bool {
        self.host.supports_avatars()
    }

    pub async fn avatar_url(
        &self,
        commit: SharedString,
        author_email: Option<SharedString>,
        client: Arc<dyn HttpClient>,
    ) -> Option<Url> {
        self.host
            .commit_author_avatar_url(&self.owner, &self.repo, commit, author_email, client)
            .await
            .ok()
            .flatten()
    }
}

pub struct BuildCommitPermalinkParams<'a> {
    pub sha: &'a str,
}

pub struct BuildPermalinkParams<'a> {
    pub sha: &'a str,
    /// URL-escaped path using unescaped `/` as the directory separator.
    pub path: String,
    pub selection: Option<Range<u32>>,
}

impl<'a> BuildPermalinkParams<'a> {
    pub fn new(sha: &'a str, path: &RepoPath, selection: Option<Range<u32>>) -> Self {
        Self {
            sha,
            path: path.components().map(urlencoding::encode).join("/"),
            selection,
        }
    }
}

/// A Git hosting provider.
#[async_trait]
pub trait GitHostingProvider {
    /// Returns the name of the provider.
    fn name(&self) -> String;

    /// Returns the base URL of the provider.
    fn base_url(&self) -> Url;

    /// Returns a permalink to a Git commit on this hosting provider.
    fn build_commit_permalink(
        &self,
        remote: &ParsedGitRemote,
        params: BuildCommitPermalinkParams,
    ) -> Url;

    /// Returns a permalink to a file and/or selection on this hosting provider.
    fn build_permalink(&self, remote: ParsedGitRemote, params: BuildPermalinkParams) -> Url;

    /// Returns a URL to create a pull request on this hosting provider.
    fn build_create_pull_request_url(
        &self,
        _remote: &ParsedGitRemote,
        _source_branch: &str,
    ) -> Option<Url> {
        None
    }

    /// Returns whether this provider supports avatars.
    fn supports_avatars(&self) -> bool;

    /// Returns a URL fragment to the given line selection.
    fn line_fragment(&self, selection: &Range<u32>) -> String {
        if selection.start == selection.end {
            let line = selection.start + 1;

            self.format_line_number(line)
        } else {
            let start_line = selection.start + 1;
            let end_line = selection.end + 1;

            self.format_line_numbers(start_line, end_line)
        }
    }

    /// Returns a formatted line number to be placed in a permalink URL.
    fn format_line_number(&self, line: u32) -> String;

    /// Returns a formatted range of line numbers to be placed in a permalink URL.
    fn format_line_numbers(&self, start_line: u32, end_line: u32) -> String;

    fn parse_remote_url(&self, url: &str) -> Option<ParsedGitRemote>;

    fn extract_pull_request(
        &self,
        _remote: &ParsedGitRemote,
        _message: &str,
    ) -> Option<PullRequest> {
        None
    }

    async fn commit_author_avatar_url(
        &self,
        _repo_owner: &str,
        _repo: &str,
        _commit: SharedString,
        _author_email: Option<SharedString>,
        _http_client: Arc<dyn HttpClient>,
    ) -> Result<Option<Url>> {
        Ok(None)
    }

    /// Returns whether this provider can fetch pull request details and checks.
    fn supports_pull_request_details(&self) -> bool {
        false
    }

    /// Begins an OAuth device authorization for the given OAuth application.
    async fn request_oauth_device_authorization(
        &self,
        _client_id: &str,
        _http_client: Arc<dyn HttpClient>,
    ) -> Result<OAuthDeviceAuthorization> {
        bail!("{} does not support signing in", self.name())
    }

    /// Checks once whether the user has completed the device authorization.
    async fn poll_oauth_access_token(
        &self,
        _client_id: &str,
        _authorization: &OAuthDeviceAuthorization,
        _http_client: Arc<dyn HttpClient>,
    ) -> Result<OAuthAccessTokenPoll> {
        bail!("{} does not support signing in", self.name())
    }

    /// Finds the most relevant pull request in `remote` whose head is
    /// `head_branch` in the repository owned by `head_owner`.
    async fn find_pull_request(
        &self,
        _remote: &ParsedGitRemote,
        _head_owner: &str,
        _head_branch: &str,
        _access_token: Option<&str>,
        _http_client: Arc<dyn HttpClient>,
    ) -> Result<Option<PullRequestDetails>> {
        bail!("{} does not support pull request details", self.name())
    }

    async fn pull_request_checks(
        &self,
        _remote: &ParsedGitRemote,
        _pull_request: &PullRequestDetails,
        _access_token: Option<&str>,
        _http_client: Arc<dyn HttpClient>,
    ) -> Result<Vec<CheckRun>> {
        bail!("{} does not support pull request checks", self.name())
    }
}

#[derive(Default, Deref, DerefMut)]
struct GlobalGitHostingProviderRegistry(Arc<GitHostingProviderRegistry>);

impl Global for GlobalGitHostingProviderRegistry {}

#[derive(Default)]
struct GitHostingProviderRegistryState {
    default_providers: Vec<Arc<dyn GitHostingProvider + Send + Sync + 'static>>,
    setting_providers: Vec<Arc<dyn GitHostingProvider + Send + Sync + 'static>>,
}

#[derive(Default)]
pub struct GitHostingProviderRegistry {
    state: RwLock<GitHostingProviderRegistryState>,
}

impl GitHostingProviderRegistry {
    /// Returns the global [`GitHostingProviderRegistry`].
    #[track_caller]
    pub fn global(cx: &App) -> Arc<Self> {
        cx.global::<GlobalGitHostingProviderRegistry>().0.clone()
    }

    /// Returns the global [`GitHostingProviderRegistry`], if one is set.
    pub fn try_global(cx: &App) -> Option<Arc<Self>> {
        cx.try_global::<GlobalGitHostingProviderRegistry>()
            .map(|registry| registry.0.clone())
    }

    /// Returns the global [`GitHostingProviderRegistry`].
    ///
    /// Inserts a default [`GitHostingProviderRegistry`] if one does not yet exist.
    pub fn default_global(cx: &mut App) -> Arc<Self> {
        cx.default_global::<GlobalGitHostingProviderRegistry>()
            .0
            .clone()
    }

    /// Sets the global [`GitHostingProviderRegistry`].
    pub fn set_global(registry: Arc<GitHostingProviderRegistry>, cx: &mut App) {
        cx.set_global(GlobalGitHostingProviderRegistry(registry));
    }

    /// Returns a new [`GitHostingProviderRegistry`].
    pub fn new() -> Self {
        Self {
            state: RwLock::new(GitHostingProviderRegistryState {
                setting_providers: Vec::default(),
                default_providers: Vec::default(),
            }),
        }
    }

    /// Returns the list of all [`GitHostingProvider`]s in the registry.
    pub fn list_hosting_providers(
        &self,
    ) -> Vec<Arc<dyn GitHostingProvider + Send + Sync + 'static>> {
        let state = self.state.read();
        state
            .default_providers
            .iter()
            .cloned()
            .chain(state.setting_providers.iter().cloned())
            .collect()
    }

    pub fn set_setting_providers(
        &self,
        providers: impl IntoIterator<Item = Arc<dyn GitHostingProvider + Send + Sync + 'static>>,
    ) {
        let mut state = self.state.write();
        state.setting_providers.clear();
        state.setting_providers.extend(providers);
    }

    /// Adds the provided [`GitHostingProvider`] to the registry.
    pub fn register_hosting_provider(
        &self,
        provider: Arc<dyn GitHostingProvider + Send + Sync + 'static>,
    ) {
        self.state.write().default_providers.push(provider);
    }
}

#[derive(Debug, PartialEq)]
pub struct ParsedGitRemote {
    pub owner: Arc<str>,
    pub repo: Arc<str>,
}

pub fn parse_git_remote_url(
    provider_registry: Arc<GitHostingProviderRegistry>,
    url: &str,
) -> Option<(
    Arc<dyn GitHostingProvider + Send + Sync + 'static>,
    ParsedGitRemote,
)> {
    provider_registry
        .list_hosting_providers()
        .into_iter()
        .find_map(|provider| {
            provider
                .parse_remote_url(url)
                .map(|parsed_remote| (provider, parsed_remote))
        })
}
