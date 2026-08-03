use iced::widget::{button, column, container, row, text, text_input};
use iced::{Element, Length, Theme};
use vida_core::i18n::I18n;

use crate::app::AppMessage;
use crate::secure_text_input::SecureTextInput;

/// ID for the unlock password input, used to focus/select-all after error.
pub const UNLOCK_PASSPHRASE_ID: &str = "unlock_passphrase";

/// Custom text input style with red border for error state.
fn error_text_input_style(theme: &Theme, status: text_input::Status) -> text_input::Style {
    // Use default styling but with red border color
    let mut style = text_input::default(theme, status);
    style.border.color = iced::Color::from_rgb(0.9, 0.2, 0.2);
    style.border.width = 2.0;
    style
}

#[derive(Debug, Clone)]
pub struct State {
    pub passphrase: String,
    pub remember: bool,
    pub error: Option<String>,
    pub error_category: Option<String>,
    pub unlocking: bool,
    /// Whether the toast notification is visible.
    pub toast_visible: bool,
}

impl State {
    pub fn new() -> Self {
        Self {
            passphrase: String::new(),
            remember: false,
            error: None,
            error_category: None,
            unlocking: false,
            toast_visible: false,
        }
    }

    pub fn view(&self, i18n: &I18n) -> Element<'_, AppMessage> {
        let title = text("vida").size(32);
        let subtitle = text(i18n.tr("unlock_title")).size(18);

        // Error styling: red border for input when error exists
        let can_unlock = !self.passphrase.is_empty() && !self.unlocking;
        
        let pass_input = if self.error.is_some() {
            let input = SecureTextInput::new(i18n.tr("unlock_passphrase_placeholder"), &self.passphrase)
                .on_input(AppMessage::UnlockPassphraseChanged)
                .secure(true)
                .style(error_text_input_style)
                .id(UNLOCK_PASSPHRASE_ID);
            if can_unlock {
                input.on_submit(AppMessage::UnlockVault)
            } else {
                input
            }
        } else {
            let input = SecureTextInput::new(i18n.tr("unlock_passphrase_placeholder"), &self.passphrase)
                .on_input(AppMessage::UnlockPassphraseChanged)
                .secure(true)
                .id(UNLOCK_PASSPHRASE_ID);
            if can_unlock {
                input.on_submit(AppMessage::UnlockVault)
            } else {
                input
            }
        };

        let remember_check = iced::widget::checkbox(self.remember)
            .label(i18n.tr("unlock_remember"))
            .on_toggle(AppMessage::UnlockRememberToggled);

        let unlock_btn = if self.unlocking {
            button(i18n.tr("unlock_unlocking"))
        } else {
            button(i18n.tr("unlock_unlock"))
        };

        let unlock_btn = if can_unlock {
            unlock_btn.on_press(AppMessage::UnlockVault)
        } else {
            unlock_btn.style(button::secondary)
        };

        // Inline unlock button with input row
        let input_row = row![pass_input, unlock_btn]
            .spacing(10)
            .align_y(iced::Alignment::Center);

        // Main centered content
        let main_content = column![title, subtitle, input_row, remember_check]
            .spacing(10)
            .padding(40)
            .max_width(400);

        let centered_main = container(main_content)
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x(Length::Fill)
            .center_y(Length::Fill);

        // Localized error message based on category
        let error_msg = match (&self.error, &self.error_category) {
            (Some(_), Some(cat)) => {
                let key = format!("error_{}", cat);
                let localized_msg = i18n.tr_dyn(&key);
                if localized_msg == key {
                    self.error.as_ref().unwrap().clone()
                } else {
                    localized_msg
                }
            }
            (Some(e), None) => e.clone(),
            _ => String::new(),
        };

        // Always use stack layout to preserve widget tree structure (avoids
        // losing focus when the toast disappears). When not visible, the toast
        // is rendered empty with a transparent background.
        let toast_widget = container(text(error_msg).color(iced::Color::WHITE).size(14))
            .padding(iced::Padding::from([10, 20]))
            .style(move |_theme: &Theme| container::Style {
                background: if self.toast_visible {
                    Some(iced::Background::Color(iced::Color::from_rgb(0.8, 0.2, 0.2)))
                } else {
                    None
                },
                border: iced::Border {
                    radius: 4.0.into(),
                    ..Default::default()
                },
                ..Default::default()
            });

        // Toast positioned below the input row. Use stack layout with
        // height(Fill) to prevent bottom rendering leak. The padding
        // vertical value approximates: center_y offset + input_row height +
        // 8px gap.  This is a rough approximation; the centered_main
        // container places content at window center, and the input_row is
        // roughly 120px below the top of centered_main (title + subtitle +
        // spacing).  We use ~140px to land just below the input.
        //
        // Left padding aligns with centered_main's max_width(400) + padding(40)
        // which gives left edge at ~center - 200px.
        iced::widget::stack![
            centered_main,
            container(toast_widget)
                .width(Length::Fill)
                .height(Length::Fill)
                .padding(iced::Padding {
                    top: 140.0,
                    bottom: 0.0,
                    left: 0.0,
                    right: 0.0,
                })
                .center_x(Length::Fill)
        ]
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
    }
}
