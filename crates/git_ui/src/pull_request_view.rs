use std::{sync::Arc, time::Duration};

use anyhow::{Context as _, Result};
use git::{
    CheckRun, CheckStatus, GitHostingProvider, GitHostingProviderRegistry,
    HostingProviderUnauthorized, OAuthAccessTokenPoll, OAuthDeviceAuthorization, ParsedGitRemote,
    PullRequestDetails, PullRequestState, parse_git_remote_url,
};
use gpui::{
    App, ClipboardItem, Entity, EventEmitter, FocusHandle, Focusable, ScrollHandle, Task,
    WeakEntity, Window,
};
use http_client::HttpClient;
use language::LanguageRegistry;
use markdown::{Markdown, MarkdownElement, MarkdownFont, MarkdownStyle};
use project::{git_store::Repository, project_settings::ProjectSettings};
use settings::Settings as _;
use ui::{Divider, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{Item, Workspace, item::ItemEvent};

use crate::git_panel::{TrackedRemote, tracked_remote};

// Checks are only polled while signed in, because unauthenticated requests
// share a small hourly rate limit.
const PENDING_CHECKS_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
const OAUTH_SLOW_DOWN_INCREMENT: Duration = Duration::from_secs(5);

type HostingProvider = Arc<dyn GitHostingProvider + Send + Sync + 'static>;

pub(crate) fn register(workspace: &mut Workspace) {
    workspace.register_action(
        |workspace, _: &zed_actions::git::ViewPullRequest, window, cx| {
            PullRequestView::open(workspace, window, cx);
        },
    );
}

/// Returns whether any of the repository's remotes is hosted on a provider
/// that can show pull request details.
pub(crate) fn repository_supports_pull_requests(repository: &Repository, cx: &App) -> bool {
    let Some(registry) = GitHostingProviderRegistry::try_global(cx) else {
        return false;
    };
    [
        repository.remote_upstream_url.as_deref(),
        repository.remote_origin_url.as_deref(),
    ]
    .into_iter()
    .flatten()
    .any(|url| {
        parse_git_remote_url(registry.clone(), url)
            .is_some_and(|(provider, _)| provider.supports_pull_request_details())
    })
}

fn credentials_url(provider: &HostingProvider) -> String {
    format!(
        "{}/zed-pull-requests",
        provider.base_url().as_str().trim_end_matches('/')
    )
}

struct PullRequestTarget {
    provider: HostingProvider,
    /// Pull requests from forks live in the upstream repository, so it is
    /// searched before the repository that the branch was pushed to.
    remotes: Vec<ParsedGitRemote>,
    head_owner: String,
    head_branch: String,
}

impl PullRequestTarget {
    fn resolve(repository: &Repository, cx: &App) -> Result<Self> {
        let TrackedRemote { branch, remote_url } = tracked_remote(repository)
            .context("Push this branch to a remote to view its pull request")?;
        let registry = GitHostingProviderRegistry::global(cx);
        let (provider, head_remote) = parse_git_remote_url(registry.clone(), &remote_url)
            .with_context(|| format!("Unsupported remote URL: {remote_url}"))?;
        anyhow::ensure!(
            provider.supports_pull_request_details(),
            "{} does not support viewing pull requests",
            provider.name()
        );

        let mut remotes = Vec::new();
        for url in [
            repository.remote_upstream_url.as_deref(),
            repository.remote_origin_url.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            if let Some((url_provider, remote)) = parse_git_remote_url(registry.clone(), url)
                && url_provider.base_url() == provider.base_url()
                && !remotes.contains(&remote)
            {
                remotes.push(remote);
            }
        }

        let head_owner = head_remote.owner.to_string();
        if !remotes.contains(&head_remote) {
            remotes.push(head_remote);
        }

        Ok(Self {
            provider,
            remotes,
            head_owner,
            head_branch: branch,
        })
    }

    async fn fetch(
        &self,
        access_token: Option<&str>,
        http_client: Arc<dyn HttpClient>,
    ) -> Result<Option<(PullRequestDetails, Result<Vec<CheckRun>>)>> {
        for remote in &self.remotes {
            let Some(pull_request) = self
                .provider
                .find_pull_request(
                    remote,
                    &self.head_owner,
                    &self.head_branch,
                    access_token,
                    http_client.clone(),
                )
                .await?
            else {
                continue;
            };

            let checks = self
                .provider
                .pull_request_checks(remote, &pull_request, access_token, http_client.clone())
                .await;
            return Ok(Some((pull_request, checks)));
        }
        Ok(None)
    }
}

struct LoadedPullRequest {
    details: PullRequestDetails,
    description: Entity<Markdown>,
    checks: Result<Vec<CheckRun>, SharedString>,
}

enum ViewState {
    Loading,
    SignInRequired { message: SharedString },
    SigningIn(OAuthDeviceAuthorization),
    NoPullRequest { head_branch: SharedString },
    Loaded(LoadedPullRequest),
    Error(SharedString),
}

pub struct PullRequestView {
    repository: WeakEntity<Repository>,
    language_registry: Arc<LanguageRegistry>,
    state: ViewState,
    signed_in: bool,
    focus_handle: FocusHandle,
    scroll_handle: ScrollHandle,
    _load_task: Task<()>,
    _refresh_timer: Task<()>,
    _sign_in_task: Task<()>,
}

impl PullRequestView {
    fn open(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
        let Some(repository) = workspace.project().read(cx).active_repository(cx) else {
            return;
        };

        let existing = workspace
            .items_of_type::<PullRequestView>(cx)
            .find(|view| view.read(cx).repository.entity_id() == repository.entity_id());
        if let Some(existing) = existing {
            workspace.activate_item(&existing, true, true, window, cx);
            existing.update(cx, |view, cx| view.refresh(cx));
            return;
        }

        let language_registry = workspace.project().read(cx).languages().clone();
        let view = cx.new(|cx| Self::new(repository.downgrade(), language_registry, cx));
        workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
    }

    fn new(
        repository: WeakEntity<Repository>,
        language_registry: Arc<LanguageRegistry>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            repository,
            language_registry,
            state: ViewState::Loading,
            signed_in: false,
            focus_handle: cx.focus_handle(),
            scroll_handle: ScrollHandle::new(),
            _load_task: Task::ready(()),
            _refresh_timer: Task::ready(()),
            _sign_in_task: Task::ready(()),
        };
        this.refresh(cx);
        this
    }

    fn resolve_target(&self, cx: &App) -> Result<PullRequestTarget> {
        self.repository
            .read_with(cx, |repository, cx| {
                PullRequestTarget::resolve(repository, cx)
            })
            .and_then(|target| target)
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let target = match self.resolve_target(cx) {
            Ok(target) => target,
            Err(error) => {
                self.set_state(ViewState::Error(format!("{error:#}").into()), cx);
                return;
            }
        };

        // Keep showing the loaded pull request while it refreshes, so that
        // background polling doesn't make the page flicker.
        if !matches!(self.state, ViewState::Loaded(_)) {
            self.set_state(ViewState::Loading, cx);
        }

        let http_client = cx.http_client();
        let credentials_provider = zed_credentials_provider::global(cx);
        self._refresh_timer = Task::ready(());
        self._load_task = cx.spawn(async move |this, cx| {
            let credentials_url = credentials_url(&target.provider);
            let access_token = credentials_provider
                .read_credentials(&credentials_url, cx)
                .await
                .log_err()
                .flatten()
                .and_then(|(_, token)| String::from_utf8(token).log_err());

            let result = target.fetch(access_token.as_deref(), http_client).await;

            let unauthorized = result.as_ref().err().and_then(|error| {
                error
                    .downcast_ref::<HostingProviderUnauthorized>()
                    .map(|error| SharedString::from(error.message.clone()))
            });
            if unauthorized.is_some() && access_token.is_some() {
                credentials_provider
                    .delete_credentials(&credentials_url, cx)
                    .await
                    .log_err();
            }

            this.update(cx, |this, cx| {
                this.signed_in = access_token.is_some() && unauthorized.is_none();
                let state = match result {
                    Ok(Some((details, checks))) => {
                        if this.signed_in
                            && checks.as_ref().is_ok_and(|checks| {
                                checks
                                    .iter()
                                    .any(|check| check.status == CheckStatus::Pending)
                            })
                        {
                            this.schedule_refresh(cx);
                        }
                        ViewState::Loaded(this.loaded_pull_request(details, checks, cx))
                    }
                    Ok(None) => ViewState::NoPullRequest {
                        head_branch: target.head_branch.into(),
                    },
                    Err(error) => match unauthorized {
                        Some(message) => ViewState::SignInRequired { message },
                        None => ViewState::Error(format!("{error:#}").into()),
                    },
                };
                this.set_state(state, cx);
            })
            .log_err();
        });
    }

    fn loaded_pull_request(
        &self,
        details: PullRequestDetails,
        checks: Result<Vec<CheckRun>>,
        cx: &mut Context<Self>,
    ) -> LoadedPullRequest {
        let description = match &self.state {
            ViewState::Loaded(loaded) if loaded.details.body == details.body => {
                loaded.description.clone()
            }
            _ => cx.new(|cx| {
                Markdown::new(
                    details.body.clone(),
                    Some(self.language_registry.clone()),
                    None,
                    cx,
                )
            }),
        };

        LoadedPullRequest {
            details,
            description,
            checks: checks.map_err(|error| format!("{error:#}").into()),
        }
    }

    fn schedule_refresh(&mut self, cx: &mut Context<Self>) {
        self._refresh_timer = cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(PENDING_CHECKS_REFRESH_INTERVAL)
                .await;
            this.update(cx, |this, cx| this.refresh(cx)).log_err();
        });
    }

    fn set_state(&mut self, state: ViewState, cx: &mut Context<Self>) {
        self.state = state;
        cx.notify();
    }

    fn sign_in(&mut self, cx: &mut Context<Self>) {
        let target = match self.resolve_target(cx) {
            Ok(target) => target,
            Err(error) => {
                self.set_state(ViewState::Error(format!("{error:#}").into()), cx);
                return;
            }
        };

        let provider = target.provider;
        let host = provider
            .base_url()
            .host_str()
            .unwrap_or_default()
            .to_string();
        let Some(client_id) = ProjectSettings::get_global(cx)
            .git
            .hosting_oauth_client_ids
            .get(&host)
            .cloned()
        else {
            self.set_state(
                ViewState::Error(
                    format!(
                        "To sign in to {}, add an OAuth app client ID for \"{host}\" \
                        to the `git.hosting_oauth_client_ids` setting.",
                        provider.name()
                    )
                    .into(),
                ),
                cx,
            );
            return;
        };

        let http_client = cx.http_client();
        let credentials_provider = zed_credentials_provider::global(cx);
        self._sign_in_task = cx.spawn(async move |this, cx| {
            let result = async {
                let authorization = provider
                    .request_oauth_device_authorization(&client_id, http_client.clone())
                    .await?;
                this.update(cx, |this, cx| {
                    this.set_state(ViewState::SigningIn(authorization.clone()), cx);
                })?;

                let mut poll_interval = authorization.poll_interval;
                let access_token = loop {
                    cx.background_executor().timer(poll_interval).await;
                    match provider
                        .poll_oauth_access_token(&client_id, &authorization, http_client.clone())
                        .await?
                    {
                        OAuthAccessTokenPoll::Pending => {}
                        OAuthAccessTokenPoll::SlowDown => {
                            poll_interval += OAUTH_SLOW_DOWN_INCREMENT;
                        }
                        OAuthAccessTokenPoll::Granted(access_token) => break access_token,
                    }
                };

                credentials_provider
                    .write_credentials(
                        &credentials_url(&provider),
                        "Bearer",
                        access_token.as_bytes(),
                        cx,
                    )
                    .await
                    .context("writing credentials to the keychain")
            }
            .await;

            this.update(cx, |this, cx| match result {
                Ok(()) => this.refresh(cx),
                Err(error) => this.set_state(ViewState::Error(format!("{error:#}").into()), cx),
            })
            .log_err();
        });
    }

    fn cancel_sign_in(&mut self, cx: &mut Context<Self>) {
        self._sign_in_task = Task::ready(());
        self.refresh(cx);
    }

    fn sign_out(&mut self, cx: &mut Context<Self>) {
        let target = match self.resolve_target(cx) {
            Ok(target) => target,
            Err(error) => {
                self.set_state(ViewState::Error(format!("{error:#}").into()), cx);
                return;
            }
        };

        let credentials_provider = zed_credentials_provider::global(cx);
        self._sign_in_task = cx.spawn(async move |this, cx| {
            let result = credentials_provider
                .delete_credentials(&credentials_url(&target.provider), cx)
                .await;
            this.update(cx, |this, cx| match result {
                Ok(()) => {
                    this.signed_in = false;
                    this.refresh(cx);
                }
                Err(error) => this.set_state(ViewState::Error(format!("{error:#}").into()), cx),
            })
            .log_err();
        });
    }

    fn render_message(
        &self,
        message: SharedString,
        action: Option<AnyElement>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_3()
            .child(
                div()
                    .max_w(rems(36.))
                    .text_center()
                    .child(Label::new(message).color(Color::Muted)),
            )
            .children(action)
            .child(self.render_refresh_button(cx))
            .into_any_element()
    }

    fn render_refresh_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        IconButton::new("refresh-pull-request", IconName::ArrowCircle)
            .icon_size(IconSize::Small)
            .tooltip(Tooltip::text("Refresh"))
            .on_click(cx.listener(|this, _, _, cx| this.refresh(cx)))
    }

    fn render_sign_in_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        Button::new("sign-in", "Sign In")
            .style(ButtonStyle::Outlined)
            .label_size(LabelSize::Small)
            .on_click(cx.listener(|this, _, _, cx| this.sign_in(cx)))
    }

    fn render_signing_in(
        &self,
        authorization: &OAuthDeviceAuthorization,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let user_code = authorization.user_code.clone();
        let verification_uri = authorization.verification_uri.clone();

        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_3()
            .child(Label::new("Enter this code to authorize Zed:").color(Color::Muted))
            .child(Headline::new(user_code.clone()).size(HeadlineSize::Large))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("copy-and-open", "Copy Code and Open Browser")
                            .style(ButtonStyle::Filled)
                            .label_size(LabelSize::Small)
                            .on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(
                                    user_code.to_string(),
                                ));
                                cx.open_url(&verification_uri);
                            }),
                    )
                    .child(
                        Button::new("cancel-sign-in", "Cancel")
                            .style(ButtonStyle::Subtle)
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(|this, _, _, cx| this.cancel_sign_in(cx))),
                    ),
            )
            .child(
                Label::new("Waiting for authorization…")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .into_any_element()
    }

    fn render_loaded(
        &self,
        loaded: &LoadedPullRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let details = &loaded.details;
        let (state_label, state_color) = match details.state {
            PullRequestState::Open => ("Open", Color::Success),
            PullRequestState::Draft => ("Draft", Color::Muted),
            PullRequestState::Merged => ("Merged", Color::Accent),
            PullRequestState::Closed => ("Closed", Color::Error),
        };
        let url = details.url.clone();

        let summary = match &details.author {
            Some(author) => format!(
                "{author} wants to merge {} into {}",
                details.head_branch, details.base_branch
            ),
            None => format!("{} into {}", details.head_branch, details.base_branch),
        };

        let header = v_flex()
            .gap_1()
            .child(
                h_flex()
                    .gap_2()
                    .justify_between()
                    .child(
                        h_flex()
                            .gap_2()
                            .min_w_0()
                            .child(
                                div()
                                    .px_1p5()
                                    .rounded_sm()
                                    .border_1()
                                    .border_color(cx.theme().colors().border)
                                    .child(
                                        Label::new(state_label)
                                            .size(LabelSize::Small)
                                            .color(state_color),
                                    ),
                            )
                            .child(Headline::new(details.title.clone()).size(HeadlineSize::Medium))
                            .child(
                                Label::new(format!("#{}", details.number))
                                    .size(LabelSize::Large)
                                    .color(Color::Muted),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .flex_none()
                            .child(
                                Button::new("open-in-browser", "Open in Browser")
                                    .end_icon(
                                        Icon::new(IconName::ArrowUpRight).size(IconSize::Small),
                                    )
                                    .label_size(LabelSize::Small)
                                    .on_click(move |_, _, cx| cx.open_url(url.as_str())),
                            )
                            .map(|this| {
                                if self.signed_in {
                                    this.child(
                                        Button::new("sign-out", "Sign Out")
                                            .label_size(LabelSize::Small)
                                            .on_click(
                                                cx.listener(|this, _, _, cx| this.sign_out(cx)),
                                            ),
                                    )
                                } else {
                                    this.child(self.render_sign_in_button(cx))
                                }
                            })
                            .child(self.render_refresh_button(cx)),
                    ),
            )
            .child(
                Label::new(summary)
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            );

        let description = if details.body.trim().is_empty() {
            Label::new("No description provided.")
                .color(Color::Muted)
                .into_any_element()
        } else {
            MarkdownElement::new(
                loaded.description.clone(),
                MarkdownStyle::themed(MarkdownFont::Preview, window, cx),
            )
            .into_any_element()
        };

        v_flex()
            .gap_4()
            .child(header)
            .child(Divider::horizontal())
            .child(
                v_flex()
                    .gap_2()
                    .child(Label::new("Description").size(LabelSize::Large))
                    .child(description),
            )
            .child(Divider::horizontal())
            .child(self.render_checks(&loaded.checks, cx))
            .into_any_element()
    }

    fn render_checks(
        &self,
        checks: &Result<Vec<CheckRun>, SharedString>,
        cx: &App,
    ) -> impl IntoElement {
        let hover_background = cx.theme().colors().element_hover;
        let content = match checks {
            Err(error) => Label::new(format!("Failed to load checks: {error}"))
                .color(Color::Error)
                .into_any_element(),
            Ok(checks) if checks.is_empty() => Label::new("No checks reported.")
                .color(Color::Muted)
                .into_any_element(),
            Ok(checks) => v_flex()
                .gap_1()
                .children(checks.iter().enumerate().map(|(index, check)| {
                    let (icon, color) = check_status_icon(check.status);
                    let url = check.url.clone();
                    h_flex()
                        .id(("check", index))
                        .gap_2()
                        .px_1()
                        .rounded_sm()
                        .child(Icon::new(icon).size(IconSize::Small).color(color))
                        .child(Label::new(check.name.clone()))
                        .when_some(check.description.clone(), |this, description| {
                            this.child(
                                Label::new(description)
                                    .size(LabelSize::Small)
                                    .color(Color::Muted)
                                    .truncate(),
                            )
                        })
                        .when_some(url, |this, url| {
                            this.cursor_pointer()
                                .hover(move |style| style.bg(hover_background))
                                .on_click(move |_, _, cx| cx.open_url(url.as_str()))
                        })
                }))
                .into_any_element(),
        };

        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .child(Label::new("Checks").size(LabelSize::Large))
                    .when_some(
                        checks.as_ref().ok().filter(|checks| !checks.is_empty()),
                        |this, checks| {
                            this.child(
                                Label::new(checks_summary(checks))
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                        },
                    ),
            )
            .child(content)
    }
}

fn check_status_icon(status: CheckStatus) -> (IconName, Color) {
    match status {
        CheckStatus::Success => (IconName::Check, Color::Success),
        CheckStatus::Failure => (IconName::XCircle, Color::Error),
        CheckStatus::Pending => (IconName::CountdownTimer, Color::Warning),
        CheckStatus::Cancelled => (IconName::Stop, Color::Muted),
        CheckStatus::Skipped => (IconName::Dash, Color::Muted),
        CheckStatus::Neutral => (IconName::Circle, Color::Muted),
    }
}

fn checks_summary(checks: &[CheckRun]) -> String {
    let count = |status: CheckStatus| checks.iter().filter(|check| check.status == status).count();
    [
        (count(CheckStatus::Failure), "failed"),
        (count(CheckStatus::Pending), "pending"),
        (count(CheckStatus::Success), "passed"),
        (
            count(CheckStatus::Cancelled)
                + count(CheckStatus::Skipped)
                + count(CheckStatus::Neutral),
            "other",
        ),
    ]
    .into_iter()
    .filter(|(count, _)| *count > 0)
    .map(|(count, label)| format!("{count} {label}"))
    .collect::<Vec<_>>()
    .join(", ")
}

impl EventEmitter<()> for PullRequestView {}

impl Focusable for PullRequestView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for PullRequestView {
    type Event = ();

    fn to_item_events(_: &Self::Event, _: &mut dyn FnMut(ItemEvent)) {}

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::PullRequest).color(Color::Muted))
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        match &self.state {
            ViewState::Loaded(loaded) => format!("PR #{}", loaded.details.number).into(),
            _ => "Pull Request".into(),
        }
    }

    fn tab_tooltip_text(&self, _cx: &App) -> Option<SharedString> {
        match &self.state {
            ViewState::Loaded(loaded) => Some(loaded.details.title.clone()),
            _ => None,
        }
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("Pull Request View Opened")
    }
}

impl Render for PullRequestView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = match &self.state {
            ViewState::Loading => v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .child(Label::new("Loading pull request…").color(Color::Muted))
                .into_any_element(),
            ViewState::SignInRequired { message } => {
                let sign_in_button = self.render_sign_in_button(cx).into_any_element();
                self.render_message(message.clone(), Some(sign_in_button), cx)
            }
            ViewState::SigningIn(authorization) => self.render_signing_in(authorization, cx),
            ViewState::NoPullRequest { head_branch } => {
                let create_button = Button::new("create-pull-request", "Create Pull Request")
                    .style(ButtonStyle::Outlined)
                    .label_size(LabelSize::Small)
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(zed_actions::git::CreatePullRequest), cx);
                    })
                    .into_any_element();
                self.render_message(
                    format!("No pull request found for {head_branch}.").into(),
                    Some(create_button),
                    cx,
                )
            }
            ViewState::Error(message) => self.render_message(message.clone(), None, cx),
            ViewState::Loaded(loaded) => self.render_loaded(loaded, window, cx),
        };

        v_flex()
            .id("pull-request-view")
            .key_context("PullRequestView")
            .track_focus(&self.focus_handle)
            .size_full()
            .p_4()
            .overflow_y_scroll()
            .track_scroll(&self.scroll_handle)
            .bg(cx.theme().colors().editor_background)
            .child(content)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use git_hosting_providers::Github;
    use http_client::{AsyncBody, FakeHttpClient, Response};
    use pretty_assertions::assert_eq;

    use super::*;

    fn check(name: &str, status: CheckStatus) -> CheckRun {
        CheckRun {
            name: name.to_string().into(),
            status,
            description: None,
            url: None,
        }
    }

    #[test]
    fn test_checks_summary() {
        let checks = [
            check("a", CheckStatus::Success),
            check("b", CheckStatus::Failure),
            check("c", CheckStatus::Success),
            check("d", CheckStatus::Skipped),
        ];
        assert_eq!(checks_summary(&checks), "1 failed, 2 passed, 1 other");
        assert_eq!(
            checks_summary(&[check("a", CheckStatus::Pending)]),
            "1 pending"
        );
    }

    #[gpui::test]
    async fn test_fetch_searches_upstream_then_fork() {
        let requested_urls = Arc::new(Mutex::new(Vec::new()));
        let http_client = FakeHttpClient::create({
            let requested_urls = requested_urls.clone();
            move |request| {
                let requested_urls = requested_urls.clone();
                async move {
                    let url = request.uri().to_string();
                    requested_urls.lock().unwrap().push(url.clone());

                    let body = if url.contains("/repos/zed-industries/zed/pulls") {
                        serde_json::json!([])
                    } else if url.contains("/repos/contributor/zed/pulls") {
                        serde_json::json!([{
                            "number": 7,
                            "title": "Fix bug",
                            "body": "Fixes the bug",
                            "state": "open",
                            "draft": false,
                            "merged_at": null,
                            "user": { "login": "contributor" },
                            "html_url": "https://github.com/contributor/zed/pull/7",
                            "head": { "ref": "fix-bug", "sha": "abc123" },
                            "base": { "ref": "main", "sha": "def456" },
                        }])
                    } else if url.contains("/check-runs") {
                        serde_json::json!({ "check_runs": [{
                            "name": "tests",
                            "status": "in_progress",
                            "conclusion": null,
                            "html_url": null,
                            "details_url": null,
                            "output": null,
                        }] })
                    } else if url.contains("/status") {
                        serde_json::json!({ "statuses": [{
                            "context": "ci/external",
                            "state": "failure",
                            "description": "Build failed",
                            "target_url": null,
                        }] })
                    } else {
                        return Ok(Response::builder().status(404).body(AsyncBody::default())?);
                    };

                    Ok(Response::builder()
                        .status(200)
                        .body(AsyncBody::from(body.to_string()))?)
                }
            }
        });

        let target = PullRequestTarget {
            provider: Arc::new(Github::public_instance()),
            remotes: vec![
                ParsedGitRemote {
                    owner: "zed-industries".into(),
                    repo: "zed".into(),
                },
                ParsedGitRemote {
                    owner: "contributor".into(),
                    repo: "zed".into(),
                },
            ],
            head_owner: "contributor".to_string(),
            head_branch: "fix-bug".to_string(),
        };

        let (details, checks) = target
            .fetch(Some("token"), http_client)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(details.number, 7);
        assert_eq!(details.state, PullRequestState::Open);

        let checks = checks.unwrap();
        assert_eq!(
            checks
                .iter()
                .map(|check| (check.name.to_string(), check.status))
                .collect::<Vec<_>>(),
            vec![
                ("ci/external".to_string(), CheckStatus::Failure),
                ("tests".to_string(), CheckStatus::Pending),
            ]
        );

        let requested_urls = requested_urls.lock().unwrap().clone();
        assert_eq!(
            requested_urls[..2],
            [
                "https://api.github.com/repos/zed-industries/zed/pulls?head=contributor%3Afix-bug&state=all&per_page=10",
                "https://api.github.com/repos/contributor/zed/pulls?head=contributor%3Afix-bug&state=all&per_page=10",
            ]
        );
        assert!(requested_urls[2..].iter().all(|url| {
            url.starts_with("https://api.github.com/repos/contributor/zed/commits/abc123/")
        }));
    }
}
