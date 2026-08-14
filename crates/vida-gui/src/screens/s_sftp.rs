use std::collections::{BTreeSet, HashMap, VecDeque};
use std::time::{Duration, Instant};

use iced::mouse;
use iced::widget::{
    Space, button, column, container, float, mouse_area, row, rule, scrollable, stack, text,
    text_input,
};
use iced::{Alignment, Element, Length, Vector};
use serde::Deserialize;
use vida_core::i18n::I18n;

use crate::app::AppMessage;
use crate::ui::{self, icons};

#[derive(Debug, Clone, Deserialize)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub permissions: String,
    pub modified: String,
}

#[derive(Debug, Clone)]
pub struct State {
    pub host_id: String,
    pub host_name: String,
    pub path: String,
    pub entries: Vec<Entry>,
    pub selected: BTreeSet<String>,
    pub selection_anchor: Option<String>,
    pub selection_base: BTreeSet<String>,
    pub selection_additive: bool,
    pub selection_dragging: bool,
    pub hovered: Option<String>,
    pub context_entry: Option<String>,
    pub cursor_position: iced::Point,
    pub context_position: Option<iced::Point>,
    pub loading: bool,
    pub loading_pulse: bool,
    pub error: Option<String>,
    pub notice: Option<String>,
    pub show_create_directory: bool,
    pub new_directory_name: String,
    pub show_filter: bool,
    pub filter: String,
    pub drag_hovered: bool,
    /// Local path plus the remote destination captured at drop/pick time.
    pub pending_uploads: VecDeque<(String, String)>,
    pub upload_in_progress: bool,
    /// Remote path, local path, recursive directory flag.
    pub pending_downloads: VecDeque<(String, String, bool)>,
    pub download_in_progress: bool,
    pub visible_limit: usize,
    cache: HashMap<String, CachedDirectory>,
    cache_order: VecDeque<String>,
}

#[derive(Debug, Clone)]
struct CachedDirectory {
    entries: Vec<Entry>,
    fetched_at: Instant,
}

pub const DIRECTORY_PAGE_SIZE: usize = 200;
const DIRECTORY_CACHE_TTL: Duration = Duration::from_secs(30);
const DIRECTORY_CACHE_CAPACITY: usize = 16;

impl State {
    pub fn new(host_id: String, host_name: String) -> Self {
        Self {
            host_id,
            host_name,
            path: ".".into(),
            entries: Vec::new(),
            selected: BTreeSet::new(),
            selection_anchor: None,
            selection_base: BTreeSet::new(),
            selection_additive: false,
            selection_dragging: false,
            hovered: None,
            context_entry: None,
            cursor_position: iced::Point::ORIGIN,
            context_position: None,
            loading: true,
            loading_pulse: false,
            error: None,
            notice: None,
            show_create_directory: false,
            new_directory_name: String::new(),
            show_filter: false,
            filter: String::new(),
            drag_hovered: false,
            pending_uploads: VecDeque::new(),
            upload_in_progress: false,
            pending_downloads: VecDeque::new(),
            download_in_progress: false,
            visible_limit: DIRECTORY_PAGE_SIZE,
            cache: HashMap::new(),
            cache_order: VecDeque::new(),
        }
    }

    /// Restores an already visited directory immediately. Returns whether the
    /// cached value is still fresh enough to skip a background read.
    pub fn restore_cached(&mut self, path: &str) -> Option<bool> {
        let cached = self.cache.get(path)?.clone();
        self.path = path.to_owned();
        self.entries = cached.entries;
        self.reset_selection();
        self.visible_limit = DIRECTORY_PAGE_SIZE;
        Some(cached.fetched_at.elapsed() <= DIRECTORY_CACHE_TTL)
    }

    pub fn remember_directory(&mut self, path: String, entries: Vec<Entry>) {
        self.cache_order.retain(|cached| cached != &path);
        self.cache_order.push_back(path.clone());
        self.cache.insert(
            path,
            CachedDirectory {
                entries,
                fetched_at: Instant::now(),
            },
        );
        while self.cache_order.len() > DIRECTORY_CACHE_CAPACITY {
            if let Some(oldest) = self.cache_order.pop_front() {
                self.cache.remove(&oldest);
            }
        }
    }

    pub fn reset_selection(&mut self) {
        self.selected.clear();
        self.selection_anchor = None;
        self.selection_base.clear();
        self.selection_dragging = false;
        self.hovered = None;
        self.context_entry = None;
        self.context_position = None;
    }

    pub fn select_range_to(&mut self, name: &str) {
        let Some(anchor) = self.selection_anchor.as_ref() else {
            return;
        };
        let Some(start) = self.entries.iter().position(|entry| &entry.name == anchor) else {
            return;
        };
        let Some(end) = self.entries.iter().position(|entry| entry.name == name) else {
            return;
        };
        let (start, end) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        let range = self.entries[start..=end]
            .iter()
            .map(|entry| entry.name.clone())
            .collect::<BTreeSet<_>>();
        self.selected = self.selection_base.union(&range).cloned().collect();
    }

    pub fn view<'a>(&'a self, i18n: &'a I18n) -> Element<'a, AppMessage> {
        let download_label = if self.selected.is_empty() {
            i18n.tr("sftp_download").to_string()
        } else {
            i18n.trf("sftp_download_count", &[&self.selected.len().to_string()])
        };
        let title = row![
            icons::icon(icons::FOLDER, 18).color(ui::ACCENT),
            text(i18n.trf("sftp_title", &[&self.host_name])).size(18),
            Space::new().width(Length::Fill),
            button(i18n.tr("sftp_filter"))
                .on_press(AppMessage::SftpToggleFilter)
                .style(ui::secondary_button)
                .padding([8, 14]),
            button(i18n.tr("sftp_new_folder"))
                .on_press(AppMessage::SftpToggleCreateDirectory)
                .style(ui::secondary_button)
                .padding([8, 14]),
            button(i18n.tr("sftp_upload"))
                .on_press(AppMessage::SftpChooseUpload)
                .style(ui::primary_button)
                .padding([8, 14]),
            button(i18n.tr("sftp_upload_folder"))
                .on_press(AppMessage::SftpChooseUploadFolder)
                .style(ui::secondary_button)
                .padding([8, 14]),
            button(text(download_label))
                .on_press_maybe(
                    (!self.selected.is_empty()).then_some(AppMessage::SftpChooseDownload)
                )
                .style(ui::secondary_button)
                .padding([8, 14]),
            button(icons::icon(icons::REFRESH, 15))
                .on_press_maybe((!self.loading).then_some(AppMessage::SftpRefresh))
                .style(ui::icon_button(false))
                .padding(9),
        ]
        .spacing(10)
        .align_y(Alignment::Center);
        let loading_status = self.loading.then(|| {
            container(
                row![
                    icons::icon(icons::REFRESH, 14).color(if self.loading_pulse {
                        ui::ACCENT_HOVER
                    } else {
                        ui::ACCENT
                    }),
                    text(i18n.tr("sftp_loading")).size(12),
                ]
                .spacing(7)
                .align_y(Alignment::Center),
            )
            .padding([7, 10])
            .style(ui::notice)
        });
        let mut breadcrumb = row![
            container(text(&self.path).size(13))
                .padding([8, 12])
                .width(Length::Fill)
                .style(ui::code_surface),
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        if let Some(status) = loading_status {
            breadcrumb = breadcrumb.push(status);
        }

        let header = row![
            text(i18n.tr("sftp_name"))
                .size(12)
                .width(Length::FillPortion(5)),
            text(i18n.tr("sftp_size"))
                .size(12)
                .width(Length::FillPortion(2)),
            text(i18n.tr("sftp_modified"))
                .size(12)
                .width(Length::FillPortion(3)),
            text(i18n.tr("sftp_permissions"))
                .size(12)
                .width(Length::FillPortion(2)),
        ]
        .spacing(10);
        let mut files = column![].spacing(3);
        if self.path != "/" {
            let parent_line = row![
                icons::icon(icons::FOLDER, 15).color(ui::ACCENT),
                text(i18n.tr("sftp_parent")).size(13),
            ]
            .spacing(9)
            .align_y(Alignment::Center);
            files = files.push(
                button(parent_line)
                    .on_press_maybe((!self.loading).then_some(AppMessage::SftpParent))
                    .style(ui::nav_item(false))
                    .padding([9, 10])
                    .width(Length::Fill),
            );
        }
        files = files.push(header).push(rule::horizontal(1));
        let normalized_filter = self.filter.trim().to_lowercase();
        let visible_entries = self
            .entries
            .iter()
            .filter(|entry| {
                normalized_filter.is_empty()
                    || entry.name.to_lowercase().contains(&normalized_filter)
            })
            .take(self.visible_limit)
            .collect::<Vec<_>>();
        let filtered_count = self
            .entries
            .iter()
            .filter(|entry| {
                normalized_filter.is_empty()
                    || entry.name.to_lowercase().contains(&normalized_filter)
            })
            .count();
        for entry in &visible_entries {
            let name = entry.name.clone();
            let target = join_remote(&self.path, &entry.name);
            let selected = self.selected.contains(&entry.name);
            let hovered = self.hovered.as_deref() == Some(entry.name.as_str());
            let size = if entry.is_dir {
                "—".into()
            } else {
                format_size(entry.size)
            };
            let line = row![
                row![
                    icons::icon(
                        if entry.is_dir {
                            icons::FOLDER
                        } else {
                            icons::FILE
                        },
                        15
                    )
                    .color(if entry.is_dir {
                        ui::ACCENT
                    } else {
                        ui::TEXT_SECONDARY
                    }),
                    text(&entry.name).size(13)
                ]
                .spacing(9)
                .width(Length::FillPortion(5)),
                text(size).size(12).width(Length::FillPortion(2)),
                text(&entry.modified).size(12).width(Length::FillPortion(3)),
                text(&entry.permissions)
                    .size(12)
                    .width(Length::FillPortion(2)),
            ]
            .spacing(10)
            .align_y(Alignment::Center);
            let row_surface = container(line)
                .padding([9, 10])
                .width(Length::Fill)
                .style(ui::file_row(selected, hovered));
            let mut area = mouse_area(row_surface)
                .on_press(AppMessage::SftpSelectionStart(name.clone()))
                .on_right_press(AppMessage::SftpContextOpen(name.clone()))
                .on_release(AppMessage::SftpSelectionFinished)
                .on_enter(AppMessage::SftpSelectionHover(name.clone()))
                .on_exit(AppMessage::SftpSelectionHoverCleared)
                .interaction(mouse::Interaction::Pointer);
            if entry.is_dir && !self.loading {
                area = area.on_double_click(AppMessage::SftpEnter(target.clone()));
            }
            files = files.push(area);
        }
        if !self.loading && filtered_count == 0 {
            files = files.push(
                ui::muted(if self.filter.trim().is_empty() {
                    i18n.tr("sftp_empty")
                } else {
                    i18n.tr("sftp_filter_empty")
                })
                .size(13),
            );
        }
        if visible_entries.len() < filtered_count {
            files = files.push(
                container(
                    text(i18n.trf(
                        "sftp_visible_count",
                        &[
                            &visible_entries.len().to_string(),
                            &filtered_count.to_string(),
                        ],
                    ))
                    .size(12),
                )
                .padding([10, 10])
                .width(Length::Fill)
                .style(ui::notice),
            );
        }
        let mut content = column![title, breadcrumb].spacing(14);
        if self.show_filter {
            content = content.push(
                text_input(i18n.tr("sftp_filter_placeholder"), &self.filter)
                    .on_input(AppMessage::SftpFilterChanged)
                    .style(ui::input)
                    .padding(10)
                    .width(Length::Fill),
            );
        }
        if self.show_create_directory {
            let create_row = row![
                text_input(
                    i18n.tr("sftp_new_folder_placeholder"),
                    &self.new_directory_name
                )
                .on_input(AppMessage::SftpNewDirectoryNameChanged)
                .on_submit(AppMessage::SftpCreateDirectory)
                .style(ui::input)
                .padding(10)
                .width(Length::Fill),
                button(i18n.tr("sftp_create"))
                    .on_press_maybe(
                        (!self.new_directory_name.trim().is_empty())
                            .then_some(AppMessage::SftpCreateDirectory)
                    )
                    .style(ui::primary_button)
                    .padding([8, 14]),
                button(i18n.tr("cancel"))
                    .on_press(AppMessage::SftpToggleCreateDirectory)
                    .style(ui::secondary_button)
                    .padding([8, 14]),
            ]
            .spacing(8)
            .align_y(Alignment::Center);
            content = content.push(create_row);
        }
        if let Some(error) = &self.error {
            content = content.push(
                container(text(error).size(12))
                    .padding(10)
                    .width(Length::Fill)
                    .style(ui::error_notice),
            );
        }
        if let Some(notice) = &self.notice {
            content = content.push(
                container(text(notice).size(12))
                    .padding(10)
                    .width(Length::Fill)
                    .style(ui::success_notice),
            );
        }
        if self.drag_hovered {
            content = content.push(
                container(
                    row![
                        icons::icon(icons::FOLDER, 17).color(ui::ACCENT),
                        text(i18n.trf("sftp_drop_upload", &[&self.path])).size(13),
                    ]
                    .spacing(9)
                    .align_y(Alignment::Center),
                )
                .padding([14, 16])
                .width(Length::Fill)
                .style(ui::drop_target),
            );
        }
        if self.upload_in_progress || !self.pending_uploads.is_empty() {
            let queued = self.pending_uploads.len() + usize::from(self.upload_in_progress);
            content = content.push(
                container(text(i18n.trf("sftp_uploading_items", &[&queued.to_string()])).size(12))
                    .padding(10)
                    .width(Length::Fill)
                    .style(ui::notice),
            );
        }
        if self.download_in_progress || !self.pending_downloads.is_empty() {
            let queued = self.pending_downloads.len() + usize::from(self.download_in_progress);
            content = content.push(
                container(
                    text(i18n.trf("sftp_downloading_items", &[&queued.to_string()])).size(12),
                )
                .padding(10)
                .width(Length::Fill)
                .style(ui::notice),
            );
        }
        content = content.push(
            container(
                scrollable(files)
                    .height(Length::Fill)
                    .on_scroll(|viewport| AppMessage::SftpScrolled(viewport.relative_offset().y)),
            )
            .padding(14)
            .height(Length::Fill)
            .style(ui::surface),
        );
        let screen = container(content)
            .padding(20)
            .width(Length::Fill)
            .height(Length::Fill)
            .style(ui::app_background);
        let Some(context_name) = self.context_entry.as_deref() else {
            return screen.into();
        };
        let Some(context_position) = self.context_position else {
            return screen.into();
        };
        let Some(context_entry) = self.entries.iter().find(|entry| entry.name == context_name)
        else {
            return screen.into();
        };
        let target = join_remote(&self.path, context_name);
        let mut actions = column![].spacing(2);
        if context_entry.is_dir {
            actions = actions.push(
                button(i18n.tr("sftp_context_open"))
                    .on_press(AppMessage::SftpEnter(target.clone()))
                    .style(ui::context_menu_item)
                    .padding([8, 12])
                    .width(Length::Fill),
            );
        }
        actions = actions
            .push(
                button(i18n.tr("sftp_context_copy_path"))
                    .on_press(AppMessage::SftpCopyRemotePath(target))
                    .style(ui::context_menu_item)
                    .padding([8, 12])
                    .width(Length::Fill),
            )
            .push(
                button(i18n.tr("sftp_context_download"))
                    .on_press(AppMessage::SftpChooseDownload)
                    .style(ui::context_menu_item)
                    .padding([8, 12])
                    .width(Length::Fill),
            );
        let menu = container(actions)
            .padding(5)
            .width(190)
            .style(ui::context_menu_surface);
        let floating_menu = float(menu).translate(move |bounds, viewport| {
            let margin = 10.0;
            let x = (context_position.x + 4.0)
                .min(viewport.x + viewport.width - bounds.width - margin)
                .max(viewport.x + margin);
            let y = (context_position.y + 4.0)
                .min(viewport.y + viewport.height - bounds.height - margin)
                .max(viewport.y + margin);
            Vector::new(x - bounds.x, y - bounds.y)
        });
        stack![screen, floating_menu]
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }
}

pub fn join_remote(base: &str, name: &str) -> String {
    if base == "/" {
        format!("/{name}")
    } else if base == "." {
        name.to_string()
    } else {
        format!("{}/{name}", base.trim_end_matches('/'))
    }
}
pub fn parent_remote(path: &str) -> String {
    if path == "." || path == "/" {
        return path.to_string();
    }
    match path.rsplit_once('/') {
        Some(("", _)) => "/".into(),
        Some((parent, _)) if !parent.is_empty() => parent.into(),
        _ => ".".into(),
    }
}
fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn remote_paths_are_joined_safely() {
        assert_eq!(join_remote("/", "tmp"), "/tmp");
        assert_eq!(parent_remote("/tmp/log"), "/tmp");
        assert_eq!(parent_remote("/root"), "/");
    }

    fn entry(name: &str) -> Entry {
        Entry {
            name: name.into(),
            is_dir: false,
            size: 0,
            permissions: "-rw-r--r--".into(),
            modified: "Aug 11 01:00".into(),
        }
    }

    #[test]
    fn drag_selection_includes_the_whole_row_range() {
        let mut state = State::new("host".into(), "Host".into());
        state.entries = vec![entry("a"), entry("b"), entry("c"), entry("d")];
        state.selection_anchor = Some("b".into());
        state.selection_base.insert("a".into());
        state.select_range_to("d");
        assert_eq!(
            state.selected,
            ["a", "b", "c", "d"]
                .into_iter()
                .map(str::to_owned)
                .collect()
        );
    }

    #[test]
    fn fresh_directory_cache_restores_entries_without_a_read() {
        let mut state = State::new("host".into(), "Host".into());
        state.remember_directory("/tmp".into(), vec![entry("cached")]);
        assert_eq!(state.restore_cached("/tmp"), Some(true));
        assert_eq!(state.path, "/tmp");
        assert_eq!(state.entries[0].name, "cached");
    }
}
