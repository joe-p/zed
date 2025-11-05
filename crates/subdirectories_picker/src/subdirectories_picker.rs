use fuzzy::{StringMatch, StringMatchCandidate};
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Subscription, Task,
    WeakEntity, Window,
};
use ordered_float::OrderedFloat;
use picker::{Picker, PickerDelegate, highlighted_match_with_paths::HighlightedMatch};
use std::sync::Arc;
use ui::{Color, Icon, IconName, ListItem, ListItemSpacing, Tooltip, prelude::*};
use workspace::{CloseIntent, ModalView, Workspace, notifications::DetachAndPromptErr};
use zed_actions::OpenSubdirectories;

pub fn init(cx: &mut App) {
    cx.on_action(|open_subdirs: &OpenSubdirectories, cx| {
        let create_new_window = open_subdirs.create_new_window;
        let base_directories = open_subdirs.base_directories.clone();
        workspace::with_active_or_new_workspace(cx, move |workspace, window, cx| {
            let Some(subdirs_picker) = workspace.active_modal::<SubdirectoriesPicker>(cx) else {
                SubdirectoriesPicker::open(
                    workspace,
                    create_new_window,
                    base_directories,
                    window,
                    cx,
                );
                return;
            };

            subdirs_picker.update(cx, |subdirs_picker, cx| {
                subdirs_picker
                    .picker
                    .update(cx, |picker, cx| picker.cycle_selection(window, cx))
            });
        });
    });
}

pub struct SubdirectoriesPicker {
    pub picker: Entity<Picker<SubdirectoriesDelegate>>,
    rem_width: f32,
    _subscription: Subscription,
}

impl ModalView for SubdirectoriesPicker {}

impl SubdirectoriesPicker {
    fn new(
        delegate: SubdirectoriesDelegate,
        rem_width: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let picker = cx.new(|cx| Picker::uniform_list(delegate, window, cx));
        let _subscription = cx.subscribe(&picker, |_, _, _, cx| cx.emit(DismissEvent));

        Self {
            picker,
            rem_width,
            _subscription,
        }
    }

    pub fn open(
        workspace: &mut Workspace,
        create_new_window: bool,
        base_directories: Option<Vec<String>>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let weak = cx.entity().downgrade();
        workspace.toggle_modal(window, cx, |window, cx| {
            let delegate = SubdirectoriesDelegate::new(weak, create_new_window, base_directories);
            Self::new(delegate, 34., window, cx)
        })
    }
}

impl EventEmitter<DismissEvent> for SubdirectoriesPicker {}

impl Focusable for SubdirectoriesPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for SubdirectoriesPicker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("SubdirectoriesPicker")
            .w(rems(self.rem_width))
            .child(self.picker.clone())
            .on_mouse_down_out(cx.listener(|this, _, window, cx| {
                this.picker.update(cx, |this, cx| {
                    this.cancel(&Default::default(), window, cx);
                })
            }))
    }
}

pub struct SubdirectoriesDelegate {
    workspace: WeakEntity<Workspace>,
    subdirectories: Vec<std::path::PathBuf>,
    selected_match_index: usize,
    matches: Vec<StringMatch>,
    create_new_window: bool,
    base_directories: Vec<std::path::PathBuf>,
}

impl SubdirectoriesDelegate {
    fn new(
        workspace: WeakEntity<Workspace>,
        create_new_window: bool,
        base_directories: Option<Vec<String>>,
    ) -> Self {
        let base_directories = if let Some(dirs_str) = base_directories {
            dirs_str
                .iter()
                .map(|s| std::path::PathBuf::from(s.trim()))
                .collect()
        } else {
            dirs::home_dir()
                .map(|dir| vec![dir])
                .unwrap_or_else(|| vec![std::path::PathBuf::from("/")])
        };

        let mut delegate = Self {
            workspace,
            subdirectories: Vec::new(),
            selected_match_index: 0,
            matches: Default::default(),
            create_new_window,
            base_directories: base_directories.clone(),
        };

        // Load subdirectories from all base directories
        for base_directory in &base_directories {
            if let Ok(entries) = std::fs::read_dir(base_directory) {
                let dirs: Vec<_> = entries
                    .filter_map(|entry| entry.ok())
                    .filter(|entry| entry.path().is_dir())
                    .map(|entry| entry.path())
                    .collect();
                delegate.subdirectories.extend(dirs);
            }
        }

        delegate
    }
}

impl EventEmitter<DismissEvent> for SubdirectoriesDelegate {}

impl PickerDelegate for SubdirectoriesDelegate {
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
            "{reuse_window} reuses this window, {create_window} opens a new one",
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
            .subdirectories
            .iter()
            .enumerate()
            .map(|(id, path)| {
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_string();
                StringMatchCandidate::new(id, &name)
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

        self.matches.sort_unstable_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.candidate_id.cmp(&b.candidate_id))
        });

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
            let path = self.subdirectories[selected_match.candidate_id].clone();
            let replace_current_window = if self.create_new_window {
                secondary
            } else {
                !secondary
            };

            workspace.update(cx, |workspace, cx| {
                let paths = vec![path];
                if replace_current_window {
                    cx.spawn_in(window, async move |workspace, cx| {
                        let continue_replacing = workspace
                            .update_in(cx, |workspace, window, cx| {
                                workspace.prepare_to_close(CloseIntent::ReplaceWindow, window, cx)
                            })?
                            .await?;
                        if continue_replacing {
                            workspace
                                .update_in(cx, |workspace, window, cx| {
                                    workspace.open_workspace_for_paths(true, paths, window, cx)
                                })?
                                .await
                        } else {
                            Ok(())
                        }
                    })
                } else {
                    workspace.open_workspace_for_paths(false, paths, window, cx)
                }
                .detach_and_prompt_err(
                    "Failed to open project",
                    window,
                    cx,
                    |_, _, _| None,
                );
            });
            cx.emit(DismissEvent);
        }
    }

    fn dismissed(&mut self, _window: &mut Window, _: &mut Context<Picker<Self>>) {}

    fn no_matches_text(&self, _window: &mut Window, _cx: &mut App) -> Option<SharedString> {
        let text = if self.subdirectories.is_empty() {
            let dirs = self
                .base_directories
                .iter()
                .map(|d| d.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            format!("No subdirectories found in: {}", dirs).into()
        } else {
            "No matches".into()
        };
        Some(text)
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let hit = self.matches.get(ix)?;
        let path = self.subdirectories.get(hit.candidate_id)?;

        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();

        let highlight_positions = hit.positions.clone();

        let highlighted_match = HighlightedMatch {
            text: file_name,
            highlight_positions,
            color: Color::Default,
        };

        Some(
            ListItem::new(ix)
                .toggle_state(selected)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .child(
                    h_flex()
                        .flex_grow()
                        .gap_3()
                        .child(
                            Icon::new(IconName::Folder)
                                .color(Color::Muted)
                                .into_any_element(),
                        )
                        .child(highlighted_match.render(window, cx)),
                )
                .tooltip({
                    let path = path.clone();
                    Tooltip::text(path.display().to_string())
                }),
        )
    }
}
