//! 调试终端屏（M2b-2）：显示并操作一个本地会话。
//!
//! 本轮不接标签页/连接面板/启动流程（M2b-3 才做）——
//! 通过 VidaApp 的 debug 入口（S3 主界面底部调试按钮）进入。

use std::sync::Arc;

use iced::widget::{button, column, container, text};
use iced::{Background, Color, Element, Length};

use crate::app::AppMessage;
use crate::term::client_grid::ClientGrid;
use crate::term::primitive::ViewportMetrics;
use crate::term::widget;

/// 终端会话状态（调试屏持有）。
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
}

impl TerminalSession {
    pub fn new(session_id: String, rows: u16, cols: u16) -> Self {
        Self {
            session_id,
            grid: ClientGrid::new(rows, cols),
            closed: false,
            exit_code: None,
            notice: None,
            viewport_metrics: Arc::new(ViewportMetrics::default()),
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
    pub fn view(&self) -> Element<'_, AppMessage> {
        let notice_el = match &self.notice {
            Some(n) => text(n).size(12),
            None => text("").size(12),
        };
        let status = if self.closed {
            let code_text = match self.exit_code {
                Some(code) => format!("会话已结束（shell 退出，exit_code={}）", code),
                None => "会话已结束（shell 退出，退出码未知）".to_string(),
            };
            text(code_text).size(12)
        } else {
            text(format!("会话 {}", self.session_id)).size(12)
        };
        let canvas = container(widget::canvas(
            self.snapshot(),
            self.viewport_metrics.clone(),
            AppMessage::TerminalInput,
            AppMessage::TerminalPaste,
            terminal_resize_message,
        ))
        .width(Length::Fill)
        .height(Length::Fill)
        .style(|_| container::Style {
            background: Some(Background::Color(Color::from_rgb8(40, 44, 52))),
            ..Default::default()
        });
        let back = button("返回").on_press(AppMessage::CloseDebugTerminal);
        let content = column![notice_el, status, canvas, back]
            .spacing(8)
            .padding(8)
            .height(Length::Fill);
        container(content)
            .height(Length::Fill)
            .width(Length::Fill)
            .into()
    }
}

fn terminal_resize_message(cols: u16, rows: u16) -> AppMessage {
    AppMessage::TerminalResize { cols, rows }
}
