//! 终端 Canvas widget（M2b-1）：把 grid 快照以 TermPrimitive 画出来。
//!
//! 重绘只由帧到达触发（iced 只在 request_redraw 时重绘）——
//! 没有新帧就没有重绘，空闲 CPU ≈ 0%。

use std::sync::Arc;

use iced::advanced::layout::{self, Layout};
use iced::advanced::renderer;
use iced::advanced::widget::{Tree, Widget};
use iced::advanced::mouse;
use iced::{Element, Length, Rectangle, Size};

use super::client_grid::ClientGrid;
use super::primitive::TermPrimitive;

/// 终端画面 widget：持有 grid 快照 + 期望的行列数。
#[derive(Debug, Clone)]
pub struct TermCanvas {
    snapshot: Arc<ClientGrid>,
    width: Length,
    height: Length,
}

impl TermCanvas {
    /// 创建终端画面。`snapshot` 是 grid 的不可变快照（Arc 共享）。
    pub fn new(snapshot: Arc<ClientGrid>) -> Self {
        Self {
            snapshot,
            width: Length::Fill,
            height: Length::Fill,
        }
    }
}

impl<Message, Theme, Renderer> Widget<Message, Theme, Renderer> for TermCanvas
where
    Renderer: iced::advanced::Renderer + iced_wgpu::primitive::Renderer,
{
    fn size(&self) -> Size<Length> {
        Size {
            width: self.width,
            height: self.height,
        }
    }

    fn layout(
        &mut self,
        _tree: &mut Tree,
        _renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        layout::Node::new(limits.max())
    }

    fn draw(
        &self,
        _tree: &Tree,
        renderer: &mut Renderer,
        _theme: &Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        // 网格为空时不绘制（避免空白 primitive 浪费）
        if self.snapshot.rows == 0 || self.snapshot.cols == 0 {
            return;
        }
        let primitive = TermPrimitive::new(self.snapshot.clone(), bounds);
        renderer.draw_primitive(bounds, primitive);
    }
}

/// 把 TermCanvas 变成 Element 的辅助函数。
pub fn canvas<'a, Message>(
    snapshot: Arc<ClientGrid>,
) -> Element<'a, Message>
where
    Message: 'a,
{
    Element::new(TermCanvas::new(snapshot))
}
