//! 调试终端屏（M2b-1）：只读显示一个本地会话的终端画面。
//!
//! 本轮不接标签页/连接面板/启动流程（M2b-3 才做）——
//! 通过 VidaApp 的 debug 入口（S3 主界面底部调试按钮）进入。

use std::sync::Arc;

use iced::widget::{button, column, container, text};
use iced::{Element, Length};

use crate::app::AppMessage;
use crate::term::client_grid::ClientGrid;
use crate::term::widget;

/// 终端会话状态（调试屏持有）。
#[derive(Debug, Clone, PartialEq)]
pub struct TerminalSession {
    pub session_id: String,
    pub grid: ClientGrid,
    /// 会话是否已结束（session_closed 事件）。
    pub closed: bool,
}

impl TerminalSession {
    pub fn new(session_id: String, rows: u16, cols: u16) -> Self {
        Self {
            session_id,
            grid: ClientGrid::new(rows, cols),
            closed: false,
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
        let status = if self.closed {
            text("会话已结束（shell 退出）").size(12)
        } else {
            text(format!("会话 {}", self.session_id)).size(12)
        };
        let canvas = widget::canvas(self.snapshot());
        let back = button("返回").on_press(AppMessage::CloseDebugTerminal);
        let content = column![status, canvas, back]
            .spacing(8)
            .padding(8)
            .height(Length::Fill);
        container(content).height(Length::Fill).width(Length::Fill).into()
    }
}
