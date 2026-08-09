use iced::widget::{button, column, container, row, scrollable, text};
use iced::{Alignment, Element, Length};
use vida_client::AgentApproval;
use vida_core::i18n::I18n;

use crate::app::AppMessage;
use crate::screens::s3_main::HostItem;
use crate::ui::{self, icons};

pub fn view<'a>(
    approvals: &'a [AgentApproval],
    selected_id: Option<&str>,
    busy_id: Option<&str>,
    notice: Option<&'a str>,
    notice_is_error: bool,
    hosts: &'a [HostItem],
    i18n: &'a I18n,
) -> Element<'a, AppMessage> {
    let header = row![
        container(icons::icon(icons::SHIELD, 20).color(ui::WARNING))
            .center_x(44)
            .center_y(44)
            .style(ui::warning_badge),
        column![
            text(i18n.tr("agent_approval_title")).size(22),
            ui::muted(i18n.trf(
                "agent_approval_pending_count",
                &[&approvals.len().to_string()],
            ))
            .size(12),
        ]
        .spacing(3),
    ]
    .spacing(12)
    .align_y(Alignment::Center);

    if approvals.is_empty() {
        let message = notice.unwrap_or_else(|| i18n.tr("agent_approval_empty"));
        return container(
            column![
                header,
                container(text(message).size(13).color(ui::TEXT_SECONDARY))
                    .padding(12)
                    .width(Length::Fill)
                    .style(ui::notice),
                button(i18n.tr("common_close"))
                    .on_press(AppMessage::CloseAgentApprovalPanel)
                    .style(ui::secondary_button)
                    .padding([9, 14]),
            ]
            .spacing(16),
        )
        .padding(22)
        .width(Length::Fill)
        .max_width(560)
        .style(ui::elevated)
        .into();
    }

    let selected = selected_id
        .and_then(|id| approvals.iter().find(|item| item.approval_id == id))
        .unwrap_or(&approvals[0]);
    let now = unix_now();
    let remaining = selected.expires_at.saturating_sub(now);
    let expired = remaining == 0;

    let approval_items = approvals.iter().map(|approval| {
        let selected_item = approval.approval_id == selected.approval_id;
        let host = target_label(approval, hosts, i18n);
        button(
            column![
                text(host).size(12),
                ui::muted(single_line_preview(&approval.command)).size(11),
            ]
            .spacing(2)
            .width(Length::Fill),
        )
        .on_press(AppMessage::SelectAgentApproval(
            approval.approval_id.clone(),
        ))
        .style(ui::nav_item(selected_item))
        .padding(9)
        .width(Length::Fill)
        .into()
    });
    let list = scrollable(column(approval_items).spacing(3))
        .height(Length::Fixed(210.0))
        .width(Length::Fixed(210.0));

    let target = target_label(selected, hosts, i18n);
    let reasons = if selected.matched_rules.is_empty() && selected.reasons.is_empty() {
        i18n.tr("agent_approval_reason_unknown").to_string()
    } else if !selected.matched_rules.is_empty() {
        selected
            .matched_rules
            .iter()
            .map(|rule| localized_rule(rule, i18n))
            .collect::<Vec<_>>()
            .join(" · ")
    } else {
        selected.reasons.join(" · ")
    };
    let countdown = if expired {
        i18n.tr("agent_approval_expired").to_string()
    } else {
        i18n.trf("agent_approval_expires_in", &[&remaining.to_string()])
    };
    let detail = column![
        row![
            text(target).size(15),
            iced::widget::Space::new().width(Length::Fill),
            text(countdown).size(12).color(if expired {
                ui::DANGER_TEXT
            } else {
                ui::WARNING
            }),
        ]
        .align_y(Alignment::Center),
        ui::muted(i18n.trf("agent_approval_session", &[&selected.session_id])).size(11),
        text(i18n.tr("agent_approval_command")).size(12),
        container(
            text(&selected.command)
                .size(14)
                .wrapping(iced::widget::text::Wrapping::WordOrGlyph),
        )
        .padding(12)
        .width(Length::Fill)
        .style(ui::code_surface),
        text(i18n.tr("agent_approval_reason")).size(12),
        container(text(reasons).size(12).color(ui::DANGER_TEXT))
            .padding(10)
            .width(Length::Fill)
            .style(ui::error_notice),
    ]
    .spacing(8)
    .width(Length::Fill);

    let busy = busy_id.is_some();
    let approve = button(if busy_id == Some(selected.approval_id.as_str()) {
        i18n.tr("agent_approval_processing")
    } else {
        i18n.tr("agent_approval_approve_once")
    })
    .style(ui::primary_button)
    .padding([9, 14]);
    let approve = if !busy && !expired {
        approve.on_press(AppMessage::ApproveAgentAction(selected.approval_id.clone()))
    } else {
        approve
    };
    let deny = button(i18n.tr("agent_approval_deny"))
        .style(ui::danger_button)
        .padding([9, 14]);
    let deny = if !busy && !expired {
        deny.on_press(AppMessage::DenyAgentAction(selected.approval_id.clone()))
    } else {
        deny
    };
    let later = button(i18n.tr("agent_approval_later"))
        .on_press(AppMessage::CloseAgentApprovalPanel)
        .style(ui::secondary_button)
        .padding([9, 14]);

    let mut content = column![
        header,
        row![list, detail].spacing(16).height(Length::Fixed(260.0)),
        row![approve, deny, later].spacing(9),
    ]
    .spacing(16)
    .width(Length::Fill);
    if let Some(notice) = notice {
        let notice = container(text(notice).size(12).color(if notice_is_error {
            ui::DANGER_TEXT
        } else {
            ui::TEXT_SECONDARY
        }))
        .padding(9)
        .width(Length::Fill);
        content = content.push(if notice_is_error {
            notice.style(ui::error_notice)
        } else {
            notice.style(ui::notice)
        });
    }

    container(content)
        .padding(22)
        .width(Length::Fill)
        .max_width(760)
        .style(ui::elevated)
        .into()
}

fn target_label(approval: &AgentApproval, hosts: &[HostItem], i18n: &I18n) -> String {
    approval
        .host_id
        .as_deref()
        .and_then(|id| hosts.iter().find(|host| host.id == id))
        .map(|host| host.name.clone())
        .unwrap_or_else(|| i18n.tr("agent_approval_local_terminal").to_string())
}

fn single_line_preview(command: &str) -> String {
    const MAX_CHARS: usize = 24;
    let mut preview: String = command.chars().take(MAX_CHARS).collect();
    if command.chars().count() > MAX_CHARS {
        preview.push('…');
    }
    preview
}

fn localized_rule(rule: &str, i18n: &I18n) -> String {
    let key = match rule {
        "recursive_delete" => "agent_rule_recursive_delete",
        "raw_disk_write" => "agent_rule_raw_disk_write",
        "format_filesystem" => "agent_rule_format_filesystem",
        "power_control" => "agent_rule_power_control",
        "delete_user" => "agent_rule_delete_user",
        "flush_firewall" => "agent_rule_flush_firewall",
        "disable_firewall" => "agent_rule_disable_firewall",
        "disable_service" => "agent_rule_disable_service",
        "delete_cluster_resource" => "agent_rule_delete_cluster_resource",
        "docker_prune" => "agent_rule_docker_prune",
        "drop_database" => "agent_rule_drop_database",
        "root_world_writable" => "agent_rule_root_world_writable",
        "fork_bomb" => "agent_rule_fork_bomb",
        "account_file_write" => "agent_rule_account_file_write",
        _ => return rule.to_string(),
    };
    i18n.tr(key).to_string()
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}
