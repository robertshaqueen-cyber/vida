use iced::widget::{button, column, container, pick_list, row, text, text_input};
use iced::{Element, Length, Theme};
use vida_core::i18n::I18n;

use crate::app::AppMessage;
use crate::secure_text_input::SecureTextInput;
use crate::ui::{self, icons};

/// ID for the unlock password input, used to focus/select-all after error.
pub const UNLOCK_PASSPHRASE_ID: &str = "unlock_passphrase";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RememberDuration {
    Never,
    OneMinute,
    FiveMinutes,
    FifteenMinutes,
    OneHour,
    OneDay,
    SevenDays,
}

impl RememberDuration {
    pub const ALL: [Self; 7] = [
        Self::Never,
        Self::OneMinute,
        Self::FiveMinutes,
        Self::FifteenMinutes,
        Self::OneHour,
        Self::OneDay,
        Self::SevenDays,
    ];

    pub fn seconds(self) -> Option<u64> {
        match self {
            Self::Never => None,
            Self::OneMinute => Some(60),
            Self::FiveMinutes => Some(5 * 60),
            Self::FifteenMinutes => Some(15 * 60),
            Self::OneHour => Some(60 * 60),
            Self::OneDay => Some(24 * 60 * 60),
            Self::SevenDays => Some(7 * 24 * 60 * 60),
        }
    }

    fn label_key(self) -> &'static str {
        match self {
            Self::Never => "unlock_remember_never",
            Self::OneMinute => "unlock_remember_1m",
            Self::FiveMinutes => "unlock_remember_5m",
            Self::FifteenMinutes => "unlock_remember_15m",
            Self::OneHour => "unlock_remember_1h",
            Self::OneDay => "unlock_remember_1d",
            Self::SevenDays => "unlock_remember_7d",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RememberOption {
    duration: RememberDuration,
    label: String,
}

impl std::fmt::Display for RememberOption {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.label)
    }
}

/// Custom text input style with red border for error state.
fn error_text_input_style(theme: &Theme, status: text_input::Status) -> text_input::Style {
    let mut style = ui::input(theme, status);
    style.border.color = ui::DANGER.scale_alpha(0.72);
    style.border.width = 1.0;
    style
}

#[derive(Debug, Clone)]
pub struct State {
    pub passphrase: String,
    pub remember_duration: RememberDuration,
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
            remember_duration: RememberDuration::SevenDays,
            error: None,
            error_category: None,
            unlocking: false,
            toast_visible: false,
        }
    }

    pub fn view(&self, i18n: &I18n) -> Element<'_, AppMessage> {
        let mark = container(icons::icon(icons::LOCK, 23).color(ui::ACCENT))
            .center_x(52)
            .center_y(52)
            .style(ui::accent_badge);
        let title = text("vida").size(28);
        let subtitle = ui::muted(i18n.tr("unlock_title")).size(14);
        let header = row![mark, column![title, subtitle].spacing(3)]
            .spacing(16)
            .align_y(iced::Alignment::Center);

        // Error styling: red border for input when error exists
        let can_unlock = !self.passphrase.is_empty() && !self.unlocking;

        let pass_input = if self.error.is_some() {
            let input =
                SecureTextInput::new(i18n.tr("unlock_passphrase_placeholder"), &self.passphrase)
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
            let input =
                SecureTextInput::new(i18n.tr("unlock_passphrase_placeholder"), &self.passphrase)
                    .on_input(AppMessage::UnlockPassphraseChanged)
                    .secure(true)
                    .style(ui::input)
                    .id(UNLOCK_PASSPHRASE_ID);
            if can_unlock {
                input.on_submit(AppMessage::UnlockVault)
            } else {
                input
            }
        };

        let remember_options: Vec<RememberOption> = RememberDuration::ALL
            .into_iter()
            .map(|duration| RememberOption {
                duration,
                label: i18n.tr(duration.label_key()).to_string(),
            })
            .collect();
        let selected_remember = remember_options
            .iter()
            .find(|option| option.duration == self.remember_duration)
            .cloned();
        let remember_picker = row![
            ui::muted(i18n.tr("unlock_remember_for")).size(13),
            pick_list(remember_options, selected_remember, |option| {
                AppMessage::UnlockRememberChanged(option.duration)
            })
            .style(ui::picker)
            .menu_style(ui::picker_menu)
            .padding(10)
            .width(Length::Fill),
        ]
        .spacing(10)
        .align_y(iced::Alignment::Center);

        let unlock_btn = if self.unlocking {
            button(i18n.tr("unlock_unlocking"))
        } else {
            button(i18n.tr("unlock_unlock"))
        };

        let unlock_btn = if can_unlock {
            unlock_btn
                .on_press(AppMessage::UnlockVault)
                .style(ui::primary_button)
        } else {
            unlock_btn.style(ui::primary_button)
        };

        // Inline unlock button with input row
        let input_row = row![pass_input, unlock_btn]
            .spacing(10)
            .align_y(iced::Alignment::Center);

        // Error message — localized by category
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

        // Fixed-height error slot: 36px when error present, 0px when empty.
        // Width matches the input field (not the full column). Achieved by
        // placing the error text in a row with a trailing spacer whose width
        // mirrors the unlock button — same structure as input_row, so the
        // text portion naturally aligns with the input.
        let error_content = row![
            icons::icon(icons::CIRCLE_ALERT, 14).color(ui::DANGER_TEXT),
            text(error_msg).size(12),
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center);
        let error_slot: Element<'_, AppMessage> = if self.toast_visible {
            row![
                container(error_content)
                    .width(Length::Fill)
                    .padding(iced::Padding::from([9, 11]))
                    .style(ui::error_notice),
                // Spacer matching unlock button width + spacing, so the
                // error container stops at the input's right edge.
                text("").width(Length::Fixed(80.0)),
            ]
            .spacing(10)
            .into()
        } else {
            // Invisible placeholder to preserve tree structure (avoids
            // losing focus when the error disappears).
            container(text(""))
                .width(Length::Fill)
                .height(Length::Fixed(0.0))
                .into()
        };

        // Main centered content — error_slot sits between input and checkbox
        let main_content = container(
            column![header, input_row, error_slot, remember_picker,]
                .spacing(12)
                .width(Length::Fill),
        )
        .padding(26)
        .width(520)
        .style(ui::elevated);

        container(main_content)
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .style(ui::app_background)
            .into()
    }
}
