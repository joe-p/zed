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
use ignore::WalkBuilder;
use menu;
use ordered_float::OrderedFloat;
use picker::{Picker, PickerDelegate};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, mpsc},
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
        directories: Vec<String>,
        rem_width: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let picker = cx.new(|cx| Picker::uniform_list(delegate, window, cx));
        let _subscription = cx.subscribe(&picker, |_, _, _, cx| cx.emit(DismissEvent));

        // Spawn task to scan git directories incrementally
        cx.spawn_in(window, async move |this, cx| {
            let scan_dirs = directories
                .into_iter()
                .map(|dir| {
                    PathBuf::from(
                        shellexpand::full(&dir)
                            .unwrap_or_else(|_| dir.clone().into())
                            .into_owned(),
                    )
                })
                .collect::<Vec<_>>();

            let scan_dir_refs = scan_dirs
                .iter()
                .map(|dir| dir.as_path())
                .collect::<Vec<_>>();

            if let Ok(rx) = scan_git_directories_streaming(&scan_dir_refs) {
                while let Ok(directory) = rx.recv() {
                    let dir_clone = directory.clone();
                    if this
                        .update_in(cx, move |this, window, cx| {
                            this.picker.update(cx, move |picker, cx| {
                                picker.delegate.add_directory(dir_clone);
                                picker.update_matches(picker.query(cx), window, cx)
                            })
                        })
                        .is_err()
                    {
                        break; // Entity was dropped, stop processing
                    }
                }
            }
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
        directories: Vec<String>,
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
        scan_directories: Vec<String>,
    ) -> Self {
        Self {
            workspace,
            directories: Vec::new(),
            selected_match_index: 0,
            matches: Default::default(),
            create_new_window,
            scan_directories: scan_directories
                .into_iter()
                .map(|d| {
                    PathBuf::from(
                        shellexpand::full(&d)
                            .unwrap_or_else(|_| d.clone().into())
                            .into_owned(),
                    )
                })
                .collect(),
        }
    }

    pub fn set_directories(&mut self, directories: Vec<PathBuf>) {
        self.directories = directories;
    }

    pub fn add_directory(&mut self, directory: PathBuf) {
        // Insert in sorted order
        let insert_pos = self
            .directories
            .binary_search_by(|existing| {
                let existing_name = existing.file_name().unwrap_or_default();
                let new_name = directory.file_name().unwrap_or_default();
                existing_name.cmp(new_name)
            })
            .unwrap_or_else(|pos| pos);

        self.directories.insert(insert_pos, directory);
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

fn scan_git_directories_streaming(
    git_paths: &[&Path],
) -> Result<std::sync::mpsc::Receiver<PathBuf>, std::io::Error> {
    let mut git_paths_iter = git_paths.iter();

    // Use WalkBuilder to scan only immediate subdirectories
    let mut walk_builder = WalkBuilder::new(
        git_paths_iter
            .next()
            .expect("git_paths should have at least one element"),
    );
    walk_builder.hidden(false);

    for path in git_paths_iter {
        walk_builder.add(path);
    }

    let walker = walk_builder.build_parallel();

    let (tx, rx) = mpsc::channel::<PathBuf>();

    // Spawn the walker in a separate thread
    std::thread::spawn(move || {
        walker.run(|| {
            let tx = tx.clone();
            Box::new(move |result| {
                match result {
                    Ok(entry) => {
                        let path = entry.path();
                        if path.join(".git").exists() {
                            if let Err(e) = tx.send(path.to_path_buf()) {
                                log::error!(
                                    "Failed to send directory path {}: {}",
                                    path.display(),
                                    e
                                );
                            }

                            return ignore::WalkState::Skip;
                        }
                    }
                    Err(e) => {
                        log::error!("Failed to read directory entry {}", e);
                    }
                }
                ignore::WalkState::Continue
            })
        });

        // Drop the original sender to signal completion
        drop(tx);
    });

    Ok(rx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::HashSet, fs};
    use tempfile::TempDir;

    fn collect_streaming_results(git_paths: &[&Path]) -> Vec<PathBuf> {
        let rx = scan_git_directories_streaming(git_paths).unwrap();
        let mut directories = Vec::new();
        while let Ok(directory) = rx.recv() {
            directories.push(directory);
        }
        directories
    }

    #[test]
    fn test_scan_git_directories_empty_dir() {
        let temp_dir = TempDir::new().unwrap();
        let result = collect_streaming_results(&[temp_dir.path()]);
        assert!(result.is_empty());
    }

    #[test]
    fn test_scan_git_directories_with_git_repo() {
        let temp_dir = TempDir::new().unwrap();
        let git_repo_path = temp_dir.path().join("test-repo");
        fs::create_dir(&git_repo_path).unwrap();
        fs::create_dir(git_repo_path.join(".git")).unwrap();

        let result = collect_streaming_results(&[temp_dir.path()]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], git_repo_path);
    }

    #[test]
    fn test_scan_git_directories_with_source_files() {
        let temp_dir = TempDir::new().unwrap();
        let project_path = temp_dir.path().join("rust-project");
        fs::create_dir(&project_path).unwrap();
        fs::create_dir(project_path.join(".git")).unwrap();
        fs::write(project_path.join("main.rs"), "fn main() {}").unwrap();

        let result = collect_streaming_results(&[temp_dir.path()]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], project_path);
    }

    #[gpui::test]
    fn test_git_directories_delegate_creation() {
        let directories = vec!["~/git".to_string()];
        let delegate = GitDirectoriesDelegate::new(WeakEntity::new_invalid(), false, directories);
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
        let delegate = GitDirectoriesDelegate::new(WeakEntity::new_invalid(), false, directories);
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
    fn test_add_directory_sorted_insertion() {
        let mut delegate = GitDirectoriesDelegate::new(WeakEntity::new_invalid(), false, vec![]);

        // Add directories in non-alphabetical order
        delegate.add_directory(PathBuf::from("/path/to/zebra"));
        delegate.add_directory(PathBuf::from("/path/to/alpha"));
        delegate.add_directory(PathBuf::from("/path/to/beta"));
        delegate.add_directory(PathBuf::from("/path/to/charlie"));

        // Verify they are stored in alphabetical order by directory name
        assert_eq!(delegate.directories.len(), 4);
        assert_eq!(delegate.directories[0], PathBuf::from("/path/to/alpha"));
        assert_eq!(delegate.directories[1], PathBuf::from("/path/to/beta"));
        assert_eq!(delegate.directories[2], PathBuf::from("/path/to/charlie"));
        assert_eq!(delegate.directories[3], PathBuf::from("/path/to/zebra"));
    }

    #[test]
    fn test_streaming_scan_git_directories() {
        let temp_dir = TempDir::new().unwrap();

        // Create multiple git repositories
        let repo1_path = temp_dir.path().join("repo-alpha");
        let repo2_path = temp_dir.path().join("repo-beta");
        let repo3_path = temp_dir.path().join("repo-gamma");

        fs::create_dir(&repo1_path).unwrap();
        fs::create_dir(repo1_path.join(".git")).unwrap();

        fs::create_dir(&repo2_path).unwrap();
        fs::create_dir(repo2_path.join(".git")).unwrap();

        fs::create_dir(&repo3_path).unwrap();
        fs::create_dir(repo3_path.join(".git")).unwrap();

        // Test that streaming returns results
        let rx = scan_git_directories_streaming(&[temp_dir.path()]).unwrap();
        let mut received_dirs = Vec::new();

        // Collect results as they come in
        while let Ok(directory) = rx.recv() {
            received_dirs.push(directory);
        }

        // Should have found all 3 repositories
        assert_eq!(received_dirs.len(), 3);

        // Verify all expected directories are present (order may vary due to parallel walker)
        let received_names: HashSet<_> = received_dirs
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap())
            .collect();

        assert!(received_names.contains("repo-alpha"));
        assert!(received_names.contains("repo-beta"));
        assert!(received_names.contains("repo-gamma"));
    }

    #[test]
    fn test_path_expansion() {
        // Test tilde expansion
        if let Some(home_dir) = dirs::home_dir() {
            if let Some(home_str) = home_dir.to_str() {
                let expand = |path: &str| {
                    shellexpand::full(path)
                        .unwrap_or_else(|_| path.into())
                        .to_string()
                };

                assert_eq!(expand("~/projects"), format!("{}/projects", home_str));
                assert_eq!(expand("~"), home_str);
                assert_eq!(expand("~/"), format!("{}/", home_str));
                assert_eq!(
                    expand("~/Documents/code"),
                    format!("{}/Documents/code", home_str)
                );

                // Test that tilde only expands at the beginning
                assert_eq!(
                    expand("/some/path~/not_expanded"),
                    "/some/path~/not_expanded"
                );
            }
        }

        // Test HOME expansion
        if let Some(home_dir) = dirs::home_dir() {
            if let Some(home_str) = home_dir.to_str() {
                let expand = |path: &str| {
                    shellexpand::full(path)
                        .unwrap_or_else(|_| path.into())
                        .to_string()
                };

                assert_eq!(expand("$HOME/projects"), format!("{}/projects", home_str));
                assert_eq!(expand("/some/path"), "/some/path");
            }
        }

        // Test USER expansion
        if let Ok(user) = std::env::var("USER") {
            let expand = |path: &str| {
                shellexpand::full(path)
                    .unwrap_or_else(|_| path.into())
                    .to_string()
            };

            assert_eq!(expand("/home/$USER/git"), format!("/home/{}/git", user));
        }
    }
}
