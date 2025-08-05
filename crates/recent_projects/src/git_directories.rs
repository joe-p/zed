//! Git Directories Modal
//!
//! This module provides a modal interface for browsing and opening directories
//! under specified directories (defaults to ~/git). It's similar to the recent projects modal but specifically
//! shows git repositories and projects in the specified directories.
//!
//! ## Features
//! - Automatically scans specified directories (defaults to ~/git) for subdirectories
//! - Intelligently detects git repositories (directories with .git folder)
//! - Detects projects with source code files
//! - Provides fuzzy search over directory names
//! - Supports opening directories in current or new window
//! - Keyboard navigation and shortcuts
//!
//! ## Usage
//! The modal is triggered by the `OpenGitDirectory` action. By default, this is
//! bound to:
//! - macOS: `Alt+Cmd+G`
//! - Linux: `Alt+Ctrl+G`
//!
//! ### Default Usage (scans ~/git)
//! ```json
//! {"action": "projects::OpenGitDirectory", "create_new_window": false}
//! ```
//!
//! ### Single Directory Usage
//! ```json
//! {"action": "projects::OpenGitDirectory", "directories": ["/path/to/projects"], "create_new_window": false}
//! ```
//!
//! ### Multiple Directories Usage
//! ```json
//! {"action": "projects::OpenGitDirectory", "directories": ["/path/to/projects", "$HOME/dev", "$HOME/work"], "create_new_window": false}
//! ```
//!
//! ### Using Environment Variables
//! ```json
//! {"action": "projects::OpenGitDirectory", "directories": ["$HOME/dev"], "create_new_window": false}
//! ```
//!
//! ## Directory Detection
//! The scanner looks for:
//! 1. Directories containing a `.git` folder (actual git repositories)
//! 2. Directories with common git files (.gitignore, .gitmodules, README.md)
//! 3. Directories containing source code files (various programming languages)
//!
//! Hidden directories (starting with '.') are automatically excluded.
//!
//! ## Keyboard Shortcuts
//! - `Enter`: Open directory in current window
//! - `Cmd+Enter` (macOS) / `Ctrl+Enter` (Linux): Open directory in new window
//! - `Escape`: Cancel and close modal
//! - Type to search/filter directories

use fuzzy::{StringMatch, StringMatchCandidate};
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Subscription, Task,
    WeakEntity, Window,
};
use menu;
use ordered_float::OrderedFloat;
use picker::{Picker, PickerDelegate};
use smol::stream::StreamExt;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use ui::{Color, Icon, IconName, Label, LabelSize, ListItem, ListItemSpacing, prelude::*};
use workspace::{ModalView, Workspace, with_active_or_new_workspace};
use zed_actions::OpenGitDirectory;

pub fn init(cx: &mut App) {
    cx.on_action(|open_git_directory: &OpenGitDirectory, cx| {
        let create_new_window = open_git_directory.create_new_window;
        let directories = open_git_directory.directories.clone();
        with_active_or_new_workspace(cx, move |workspace, window, cx| {
            let Some(git_directories) = workspace.active_modal::<GitDirectories>(cx) else {
                GitDirectories::open(workspace, create_new_window, directories, window, cx);
                return;
            };

            git_directories.update(cx, |git_directories, cx| {
                git_directories
                    .picker
                    .update(cx, |picker, cx| picker.cycle_selection(window, cx))
            });
        });
    });
}

pub struct GitDirectories {
    pub picker: Entity<Picker<GitDirectoriesDelegate>>,
    rem_width: f32,
    _subscription: Subscription,
}

impl ModalView for GitDirectories {}

impl GitDirectories {
    fn new(
        delegate: GitDirectoriesDelegate,
        directories: Option<Vec<String>>,
        rem_width: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let picker = cx.new(|cx| Picker::uniform_list(delegate, window, cx));
        let _subscription = cx.subscribe(&picker, |_, _, _, cx| cx.emit(DismissEvent));

        // Spawn task to scan git directories
        cx.spawn_in(window, async move |this, cx| {
            let scan_dirs = if let Some(custom_dirs) = directories {
                custom_dirs
                    .into_iter()
                    .map(|dir| PathBuf::from(expand_path(&dir)))
                    .collect::<Vec<_>>()
            } else {
                match dirs::home_dir() {
                    Some(home_dir) => vec![home_dir.join("git")],
                    None => {
                        log::warn!("Could not determine home directory");
                        Vec::new()
                    }
                }
            };

            let mut all_directories = Vec::new();
            for scan_dir in scan_dirs {
                if scan_dir.exists() && scan_dir.is_dir() {
                    let mut found_dirs = scan_git_directories(&scan_dir).await.unwrap_or_default();
                    all_directories.append(&mut found_dirs);
                } else {
                    log::info!("Directory not found at {}", scan_dir.display());
                }
            }
            this.update_in(cx, move |this, window, cx| {
                this.picker.update(cx, move |picker, cx| {
                    picker.delegate.set_directories(all_directories);
                    picker.update_matches(picker.query(cx), window, cx)
                })
            })
            .ok()
        })
        .detach();

        Self {
            picker,
            rem_width,
            _subscription,
        }
    }

    pub fn open(
        workspace: &mut Workspace,
        create_new_window: bool,
        directories: Option<Vec<String>>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let weak = cx.entity().downgrade();
        workspace.toggle_modal(window, cx, |window, cx| {
            let delegate =
                GitDirectoriesDelegate::new(weak, create_new_window, directories.clone());
            Self::new(delegate, directories, 34., window, cx)
        })
    }
}

impl EventEmitter<DismissEvent> for GitDirectories {}

impl Focusable for GitDirectories {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for GitDirectories {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .w(rems(self.rem_width))
            .child(self.picker.clone())
            .on_mouse_down_out(cx.listener(|this, _, window, cx| {
                this.picker.update(cx, |this, cx| {
                    this.cancel(&Default::default(), window, cx);
                })
            }))
    }
}

pub struct GitDirectoriesDelegate {
    workspace: WeakEntity<Workspace>,
    directories: Vec<PathBuf>,
    selected_match_index: usize,
    matches: Vec<StringMatch>,
    create_new_window: bool,
    scan_directories: Vec<PathBuf>,
}

impl GitDirectoriesDelegate {
    fn new(
        workspace: WeakEntity<Workspace>,
        create_new_window: bool,
        scan_directories: Option<Vec<String>>,
    ) -> Self {
        Self {
            workspace,
            directories: Vec::new(),
            selected_match_index: 0,
            matches: Default::default(),
            create_new_window,
            scan_directories: scan_directories
                .unwrap_or_else(|| vec!["~/git".to_string()])
                .into_iter()
                .map(|d| PathBuf::from(expand_path(&d)))
                .collect(),
        }
    }

    pub fn set_directories(&mut self, directories: Vec<PathBuf>) {
        self.directories = directories;
    }
}

impl EventEmitter<DismissEvent> for GitDirectoriesDelegate {}

impl PickerDelegate for GitDirectoriesDelegate {
    type ListItem = ListItem;

    fn placeholder_text(&self, window: &mut Window, _: &mut App) -> Arc<str> {
        let (create_window, reuse_window) = if self.create_new_window {
            (
                window.keystroke_text_for(&menu::Confirm),
                window.keystroke_text_for(&menu::SecondaryConfirm),
            )
        } else {
            (
                window.keystroke_text_for(&menu::SecondaryConfirm),
                window.keystroke_text_for(&menu::Confirm),
            )
        };
        Arc::from(format!(
            "Open git directory - {reuse_window} reuses this window, {create_window} opens a new one",
        ))
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_match_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_match_index = ix;
    }

    fn update_matches(
        &mut self,
        query: String,
        _: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> gpui::Task<()> {
        let query = query.trim_start();
        let smart_case = query.chars().any(|c| c.is_uppercase());
        let candidates = self
            .directories
            .iter()
            .enumerate()
            .map(|(id, path)| {
                let file_name = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.to_string_lossy().into_owned());
                StringMatchCandidate::new(id, &file_name)
            })
            .collect::<Vec<_>>();

        self.matches = smol::block_on(fuzzy::match_strings(
            candidates.as_slice(),
            query,
            smart_case,
            true,
            100,
            &Default::default(),
            cx.background_executor().clone(),
        ));

        self.selected_match_index = self
            .matches
            .iter()
            .enumerate()
            .rev()
            .max_by_key(|(_, m)| OrderedFloat(m.score))
            .map(|(ix, _)| ix)
            .unwrap_or(0);

        Task::ready(())
    }

    fn confirm(&mut self, secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        if let Some((selected_match, workspace)) = self
            .matches
            .get(self.selected_index())
            .zip(self.workspace.upgrade())
        {
            let directory_path = &self.directories[selected_match.candidate_id];
            let replace_current_window = if self.create_new_window {
                secondary
            } else {
                !secondary
            };

            workspace
                .update(cx, |workspace, cx| {
                    let paths = vec![directory_path.clone()];
                    if replace_current_window {
                        workspace.open_workspace_for_paths(true, paths, window, cx)
                    } else {
                        workspace.open_workspace_for_paths(false, paths, window, cx)
                    }
                })
                .detach_and_log_err(cx);
            cx.emit(DismissEvent);
        }
    }

    fn dismissed(&mut self, _window: &mut Window, _: &mut Context<Picker<Self>>) {}

    fn no_matches_text(&self, _window: &mut Window, _cx: &mut App) -> Option<SharedString> {
        let text = if self.directories.is_empty() {
            if self.scan_directories.is_empty() {
                "No scan directories specified".into()
            } else if self.scan_directories.len() == 1 {
                format!("No git directories found in {} (create the directory and clone some repositories)", self.scan_directories[0].display()).into()
            } else {
                let dirs_str = self
                    .scan_directories
                    .iter()
                    .map(|d| d.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("No git directories found in {} (create the directories and clone some repositories)", dirs_str).into()
            }
        } else {
            "No matches".into()
        };
        Some(text)
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let hit = self.matches.get(ix)?;
        let directory_path = self.directories.get(hit.candidate_id)?;

        let file_name = directory_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| directory_path.to_string_lossy().into_owned());

        Some(
            ListItem::new(ix)
                .toggle_state(selected)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .child(
                    h_flex()
                        .gap_3()
                        .child(Icon::new(IconName::Folder).color(Color::Muted))
                        .child(
                            v_flex().child(Label::new(file_name)).child(
                                Label::new(directory_path.to_string_lossy().to_string())
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            ),
                        ),
                ),
        )
    }
}

async fn scan_git_directories(git_path: &Path) -> Result<Vec<PathBuf>, std::io::Error> {
    use smol::fs;

    if !git_path.exists() {
        log::debug!("Git path does not exist: {}", git_path.display());
        return Ok(Vec::new());
    }

    if !git_path.is_dir() {
        log::warn!("Git path is not a directory: {}", git_path.display());
        return Ok(Vec::new());
    }

    let mut directories = Vec::new();
    let mut entries = match fs::read_dir(git_path).await {
        Ok(entries) => entries,
        Err(e) => {
            log::error!("Failed to read git directory {}: {}", git_path.display(), e);
            return Err(e);
        }
    };

    while let Some(entry) = entries.next().await {
        let entry = entry?;
        let path = entry.path();

        if path.is_dir() {
            // Skip hidden directories (starting with .)
            if let Some(file_name) = path.file_name() {
                if !file_name.to_string_lossy().starts_with('.') {
                    // Check if this directory contains a .git subdirectory or is a git repository
                    let git_dir = path.join(".git");
                    if git_dir.exists() || is_likely_git_repo(&path).await {
                        directories.push(path);
                    } else {
                        // If it's not a git repo, add it anyway as it might contain projects
                        directories.push(path);
                    }
                }
            }
        }
    }

    // Sort directories by name
    directories.sort_by(|a, b| {
        let a_name = a.file_name().unwrap_or_default();
        let b_name = b.file_name().unwrap_or_default();
        a_name.cmp(b_name)
    });

    Ok(directories)
}

async fn is_likely_git_repo(path: &Path) -> bool {
    use smol::fs;

    // Check for common git-related files/directories
    let git_indicators = [
        ".git",
        ".gitignore",
        ".gitmodules",
        "README.md",
        "README.txt",
    ];

    for indicator in &git_indicators {
        if path.join(indicator).exists() {
            return true;
        }
    }

    // Check if it contains source code files (common extensions)
    if let Ok(mut entries) = fs::read_dir(path).await {
        while let Some(Ok(entry)) = entries.next().await {
            if let Some(extension) = entry.path().extension() {
                let ext = extension.to_string_lossy().to_lowercase();
                if matches!(
                    ext.as_str(),
                    "rs" | "js"
                        | "ts"
                        | "py"
                        | "go"
                        | "java"
                        | "cpp"
                        | "c"
                        | "h"
                        | "swift"
                        | "kt"
                        | "rb"
                        | "php"
                        | "cs"
                        | "dart"
                        | "vue"
                        | "jsx"
                        | "tsx"
                ) {
                    return true;
                }
            }
        }
    }

    false
}

/// Expands environment variables in a path string
/// Currently supports $HOME and $USER variables
fn expand_path(path: &str) -> String {
    let mut expanded = path.to_string();

    // Expand $HOME
    if let Some(home_dir) = dirs::home_dir() {
        if let Some(home_str) = home_dir.to_str() {
            expanded = expanded.replace("$HOME", home_str);
        }
    }

    // Expand $USER
    if let Ok(user) = std::env::var("USER") {
        expanded = expanded.replace("$USER", &user);
    }

    expanded
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_scan_git_directories_empty_dir() {
        let temp_dir = TempDir::new().unwrap();
        let result = smol::block_on(scan_git_directories(temp_dir.path())).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_scan_git_directories_with_git_repo() {
        let temp_dir = TempDir::new().unwrap();
        let git_repo_path = temp_dir.path().join("test-repo");
        fs::create_dir(&git_repo_path).unwrap();
        fs::create_dir(git_repo_path.join(".git")).unwrap();

        let result = smol::block_on(scan_git_directories(temp_dir.path())).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], git_repo_path);
    }

    #[test]
    fn test_scan_git_directories_with_source_files() {
        let temp_dir = TempDir::new().unwrap();
        let project_path = temp_dir.path().join("rust-project");
        fs::create_dir(&project_path).unwrap();
        fs::write(project_path.join("main.rs"), "fn main() {}").unwrap();

        let result = smol::block_on(scan_git_directories(temp_dir.path())).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], project_path);
    }

    #[test]
    fn test_scan_git_directories_skips_hidden() {
        let temp_dir = TempDir::new().unwrap();
        let hidden_dir = temp_dir.path().join(".hidden");
        fs::create_dir(&hidden_dir).unwrap();
        fs::create_dir(hidden_dir.join(".git")).unwrap();

        let result = smol::block_on(scan_git_directories(temp_dir.path())).unwrap();
        assert!(result.is_empty());
    }

    #[gpui::test]
    fn test_git_directories_delegate_creation() {
        let delegate = GitDirectoriesDelegate::new(WeakEntity::new_invalid(), false, None);
        assert_eq!(delegate.directories.len(), 0);
        assert_eq!(delegate.matches.len(), 0);
        assert_eq!(delegate.selected_match_index, 0);
        assert!(!delegate.create_new_window);
        assert_eq!(delegate.scan_directories.len(), 1);
        assert!(
            delegate.scan_directories[0]
                .to_string_lossy()
                .ends_with("git")
        );
    }

    #[gpui::test]
    fn test_git_directories_delegate_with_multiple_directories() {
        let directories = vec!["$HOME/work".to_string(), "$HOME/personal".to_string()];
        let delegate =
            GitDirectoriesDelegate::new(WeakEntity::new_invalid(), false, Some(directories));
        assert_eq!(delegate.directories.len(), 0);
        assert_eq!(delegate.matches.len(), 0);
        assert_eq!(delegate.selected_match_index, 0);
        assert!(!delegate.create_new_window);
        assert_eq!(delegate.scan_directories.len(), 2);
        assert!(
            delegate.scan_directories[0]
                .to_string_lossy()
                .ends_with("work")
        );
        assert!(
            delegate.scan_directories[1]
                .to_string_lossy()
                .ends_with("personal")
        );
    }

    #[test]
    fn test_scan_multiple_git_directories() {
        use std::fs;
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();

        // Create first directory with git repo
        let work_dir = temp_dir.path().join("work");
        fs::create_dir(&work_dir).unwrap();
        let git_repo1 = work_dir.join("project1");
        fs::create_dir(&git_repo1).unwrap();
        fs::create_dir(git_repo1.join(".git")).unwrap();

        // Create second directory with source files
        let personal_dir = temp_dir.path().join("personal");
        fs::create_dir(&personal_dir).unwrap();
        let project2 = personal_dir.join("project2");
        fs::create_dir(&project2).unwrap();
        fs::write(project2.join("main.rs"), "fn main() {}").unwrap();

        // Scan both directories
        let mut all_directories = Vec::new();
        for scan_dir in [&work_dir, &personal_dir] {
            if scan_dir.exists() && scan_dir.is_dir() {
                let found_dirs = smol::block_on(scan_git_directories(scan_dir)).unwrap_or_default();
                all_directories.extend(found_dirs);
            }
        }

        assert_eq!(all_directories.len(), 2);
        assert!(all_directories.contains(&git_repo1));
        assert!(all_directories.contains(&project2));
    }

    #[test]
    fn test_expand_path() {
        // Test HOME expansion
        if let Some(home_dir) = dirs::home_dir() {
            if let Some(home_str) = home_dir.to_str() {
                assert_eq!(
                    expand_path("$HOME/projects"),
                    format!("{}/projects", home_str)
                );
                assert_eq!(expand_path("/some/path"), "/some/path");
            }
        }

        // Test USER expansion
        if let Ok(user) = std::env::var("USER") {
            assert_eq!(
                expand_path("/home/$USER/git"),
                format!("/home/{}/git", user)
            );
        }
    }
}
