//! 正式终端标签内容（M2b-3）：显示并操作一个本地会话。

use std::sync::Arc;

use iced::widget::{Space, column, container, row, text};
use iced::{Alignment, Background, Color, Element, Length};
use vida_core::i18n::I18n;

use crate::app::AppMessage;
use crate::term::client_grid::ClientGrid;
use crate::term::primitive::{TerminalAppearance, ViewportMetrics};
use crate::term::widget;
use crate::ui::{self, icons};

/// 一个正式终端标签持有的会话状态。
#[derive(Debug, Clone)]
pub struct TerminalSession {
    pub session_id: String,
    pub grid: ClientGrid,
    /// 会话是否已结束（session_closed 事件）。
    pub closed: bool,
    /// 退出码（None = 未知，如被信号终止）。
    pub exit_code: Option<u32>,
    /// 需要向用户展示的提示（如「原会话已结束，已为你打开新终端」）。
    pub notice: Option<String>,
    /// 渲染器实测的 cell 物理像素尺寸，供交互 widget 计算行列数。
    pub viewport_metrics: Arc<ViewportMetrics>,
    pub appearance: TerminalAppearance,
    pub cursor_on: bool,
}

impl TerminalSession {
    pub fn new(session_id: String, rows: u16, cols: u16, appearance: TerminalAppearance) -> Self {
        Self {
            session_id,
            grid: ClientGrid::new(rows, cols),
            closed: false,
            exit_code: None,
            notice: None,
            viewport_metrics: Arc::new(ViewportMetrics::default()),
            appearance,
            cursor_on: true,
        }
    }

    /// 应用一帧推送（全量帧调用方先 reset，增量帧直接应用）。
    pub fn apply_frame(&mut self, frame: &crate::term::frame::TerminalFrame) {
        self.grid.apply_frame(frame);
    }

    pub fn snapshot(&self) -> Arc<ClientGrid> {
        Arc::new(self.grid.clone())
    }
}

impl TerminalSession {
    /// 渲染终端画面 + 会话状态栏。
    pub fn view<'a>(&'a self, i18n: &'a I18n) -> Element<'a, AppMessage> {
        let status = if self.closed {
            let code_text = match self.exit_code {
                Some(code) => i18n.trf("terminal_closed_code", &[&code.to_string()]),
                None => i18n.tr("terminal_closed_unknown").to_string(),
            };
            text(code_text).size(11).color(ui::DANGER)
        } else {
            ui::muted(i18n.trf("terminal_session_status", &[&self.session_id])).size(11)
        };
        let toolbar = container(
            row![
                icons::icon(icons::TERMINAL, 15).color(ui::ACCENT),
                text(i18n.tr("terminal_local_title")).size(13),
                Space::new().width(Length::Fill),
                status,
            ]
            .spacing(9)
            .align_y(Alignment::Center),
        )
        .height(40)
        .padding([3, 10])
        .width(Length::Fill)
        .style(ui::chrome);
        let canvas = container(widget::canvas(
            self.snapshot(),
            self.viewport_metrics.clone(),
            AppMessage::TerminalInput,
            AppMessage::TerminalPaste,
            terminal_resize_message,
            self.appearance.clone(),
            self.cursor_on,
        ))
        .width(Length::Fill)
        .height(Length::Fill)
        .style(|_| container::Style {
            background: Some(Background::Color(Color::from_rgb8(40, 44, 52))),
            ..Default::default()
        });
        let notice_el: Element<'_, AppMessage> = match &self.notice {
            Some(n) => container(text(n).size(12))
                .padding([7, 12])
                .width(Length::Fill)
                .style(ui::surface)
                .into(),
            None => container(Space::new()).height(0).into(),
        };
        let content = column![toolbar, notice_el, canvas]
            .spacing(0)
            .height(Length::Fill);
        container(content)
            .height(Length::Fill)
            .width(Length::Fill)
            .style(ui::app_background)
            .into()
    }
}

fn terminal_resize_message(cols: u16, rows: u16) -> AppMessage {
    AppMessage::TerminalResize { cols, rows }
}
