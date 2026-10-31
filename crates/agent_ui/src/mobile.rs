//! Exposes agent threads to the mobile web app (`mobile_bridge`): snapshots
//! in the app's wire format, and the actions its thread view can take. Actions
//! go through the same `ThreadView` code paths as Zed's own UI, so the desktop
//! stays in sync.

use std::collections::BTreeMap;
use std::ops::Range;

use acp_thread::{
    AcpThread, AgentModelList, AgentThreadEntry, AssistantMessageChunk, ElicitationEntryId,
    ElicitationStatus, PermissionOptions, ThreadStatus, ToolCall, ToolCallContent, ToolCallStatus,
};
use agent_client_protocol::schema::{v1 as acp_v1, v2 as acp_v2};
use agent_settings::{AgentProfile, AgentProfileId};
use anyhow::{Context as _, Result, anyhow};
use gpui::{AnyWindowHandle, App, Entity, Task};
use language_model::Speed;
use mobile_protocol::{api, rest};
use workspace::{MultiWorkspace, Workspace};

use crate::agent_panel::CreateThreadOptions;
use crate::conversation_view::ThreadView;
use crate::{Agent, AgentPanel, AgentThreadSource, ConversationView, ThreadId};

pub const MODEL_CONTROL: &str = "model";
pub const MODE_CONTROL: &str = "mode";
pub const PROFILE_CONTROL: &str = "profile";
pub const THINKING_CONTROL: &str = "thinking";
pub const EFFORT_CONTROL: &str = "effort";
pub const FAST_CONTROL: &str = "fast";
pub const CONFIG_CONTROL_PREFIX: &str = "config:";

#[derive(Clone)]
pub struct MobileConversation {
    pub window: AnyWindowHandle,
    pub workspace: Entity<Workspace>,
    pub conversation: Entity<ConversationView>,
}

impl MobileConversation {
    pub fn thread_view(&self, cx: &App) -> Option<Entity<ThreadView>> {
        self.conversation.read(cx).root_thread_view()
    }

    /// The thread is only available once its session finished loading.
    pub fn thread(&self, cx: &App) -> Option<Entity<AcpThread>> {
        Some(self.thread_view(cx)?.read(cx).thread.clone())
    }
}

/// Finds the live conversation for a thread in any window's agent panels.
pub fn find_conversation(thread_id: ThreadId, cx: &App) -> Option<MobileConversation> {
    cx.windows().into_iter().find_map(|window| {
        let multi_workspace = window.downcast::<MultiWorkspace>()?.read(cx).ok()?;
        multi_workspace.workspaces().find_map(|workspace| {
            let panel = workspace.read(cx).panel::<AgentPanel>(cx)?;
            let conversation = panel
                .read(cx)
                .conversation_view_for_id(&thread_id, cx)?
                .clone();
            Some(MobileConversation {
                window,
                workspace: workspace.clone(),
                conversation,
            })
        })
    })
}

/// Creates a thread in the workspace's agent panel without changing what the
/// desktop shows. It stays a draft until its first message is sent.
pub fn create_thread(
    workspace: &Entity<Workspace>,
    window: AnyWindowHandle,
    agent_id: &str,
    cx: &mut App,
) -> Result<ThreadId> {
    let panel = workspace
        .read(cx)
        .panel::<AgentPanel>(cx)
        .context("the workspace has no agent panel")?;
    let agent = if agent_id == agent::ZED_AGENT_ID.as_ref() {
        Agent::NativeAgent
    } else {
        let is_known = workspace
            .read(cx)
            .project()
            .read(cx)
            .agent_server_store()
            .read(cx)
            .external_agents()
            .any(|known_id| known_id.0.as_ref() == agent_id);
        if !is_known {
            return Err(anyhow!("unknown agent {agent_id:?}"));
        }
        Agent::Custom {
            id: project::AgentId(agent_id.to_string().into()),
        }
    };
    window.update(cx, |_, window, cx| {
        panel.update(cx, |panel, cx| {
            panel.create_thread_with_options(
                CreateThreadOptions {
                    title: None,
                    initial_content: None,
                    agent: Some(agent),
                    model: None,
                    work_dirs: None,
                },
                AgentThreadSource::AgentPanel,
                window,
                cx,
            )
        })
    })
}

pub fn thread_entries(conversation: &MobileConversation, cx: &App) -> Vec<api::ThreadEntry> {
    let Some(thread) = conversation.thread(cx) else {
        return Vec::new();
    };
    thread
        .read(cx)
        .entries()
        .iter()
        .map(|entry| convert_entry(entry, cx))
        .collect()
}

pub fn thread_entry(
    conversation: &MobileConversation,
    index: usize,
    cx: &App,
) -> Option<api::ThreadEntry> {
    let thread = conversation.thread(cx)?;
    let entry = thread.read(cx).entries().get(index)?;
    Some(convert_entry(entry, cx))
}

pub fn thread_status(thread: &AcpThread) -> api::ThreadStatus {
    if thread.is_waiting_for_confirmation() {
        api::ThreadStatus::WaitingForConfirmation
    } else if thread.status() == ThreadStatus::Generating {
        api::ThreadStatus::Running
    } else if thread.had_error() {
        api::ThreadStatus::Error
    } else {
        api::ThreadStatus::Idle
    }
}

pub fn thread_state(
    conversation: &MobileConversation,
    thread_id: &str,
    cx: &App,
) -> Option<api::ThreadState> {
    let thread_view = conversation.thread_view(cx)?;
    let thread_view = thread_view.read(cx);
    let conversation_view = conversation.conversation.read(cx);
    let thread = thread_view.thread.read(cx);

    let title = conversation_view
        .title_override(cx)
        .or_else(|| thread.title())
        .unwrap_or_else(|| crate::DEFAULT_THREAD_TITLE.into());
    let plan = thread
        .plan()
        .map(|plan| {
            plan.entries
                .iter()
                .map(|entry| api::PlanEntry {
                    content: entry.source.content.clone(),
                    status: match entry.source.status {
                        acp_v2::PlanEntryStatus::InProgress => api::PlanEntryStatus::InProgress,
                        acp_v2::PlanEntryStatus::Completed => api::PlanEntryStatus::Completed,
                        _ => api::PlanEntryStatus::Pending,
                    },
                })
                .collect()
        })
        .unwrap_or_default();
    let queued_messages = thread_view
        .message_queue
        .iter()
        .map(|entry| entry.editor.read(cx).text(cx))
        .collect();
    let token_usage = thread.token_usage().map(|usage| api::TokenUsage {
        used: usage.used_tokens,
        max: usage.max_tokens,
    });
    let needs_desktop_input = thread
        .entries()
        .iter()
        .any(|entry| matches!(entry, AgentThreadEntry::Elicitation(_)))
        && thread_view.thread.read(cx).status() == ThreadStatus::Generating;

    Some(api::ThreadState {
        thread_id: thread_id.to_string(),
        title: title.to_string(),
        agent_name: thread_view.agent_display_name.to_string(),
        status: thread_status(thread),
        controls: thread_controls(thread_view, cx),
        plan,
        queued_messages,
        token_usage,
        error: thread_view.error_message(cx),
        needs_desktop_input,
    })
}

fn convert_entry(entry: &AgentThreadEntry, cx: &App) -> api::ThreadEntry {
    match entry {
        AgentThreadEntry::UserMessage(message) => api::ThreadEntry::User {
            text: message.content.to_markdown(cx),
        },
        AgentThreadEntry::AssistantMessage(message) => api::ThreadEntry::Assistant {
            chunks: message
                .chunks
                .iter()
                .map(|chunk| match chunk {
                    AssistantMessageChunk::Message { block, .. } => api::AssistantChunk::Message {
                        markdown: block.to_markdown(cx),
                    },
                    AssistantMessageChunk::Thought { block, .. } => api::AssistantChunk::Thought {
                        markdown: block.to_markdown(cx),
                    },
                })
                .collect(),
        },
        AgentThreadEntry::ToolCall(tool_call) => {
            let permission_options = tool_call
                .authorization()
                .map(|authorization| permission_options(&authorization.options));
            api::ThreadEntry::ToolCall(api::ToolCall {
                id: tool_call.id.0.to_string(),
                title: tool_call.label.read(cx).source().to_string(),
                tool_kind: tool_kind_name(tool_call.kind()).to_string(),
                status: tool_call_status(tool_call.status()),
                content: tool_call
                    .content()
                    .iter()
                    .map(|content| convert_tool_call_content(content, cx))
                    .collect(),
                permission_options,
            })
        }
        AgentThreadEntry::Elicitation(_) => api::ThreadEntry::Elicitation {
            message: "The agent is asking for input.".to_string(),
        },
        AgentThreadEntry::ContextCompaction(compaction) => {
            let summary = compaction
                .summary
                .iter()
                .map(|block| block.to_markdown(cx).to_string())
                .collect::<Vec<_>>()
                .join("\n\n");
            api::ThreadEntry::Compaction {
                summary: (!summary.is_empty()).then_some(summary),
            }
        }
    }
}

fn convert_tool_call_content(content: &ToolCallContent, cx: &App) -> api::ToolCallContent {
    match content {
        ToolCallContent::Diff(diff) | ToolCallContent::LegacyDiff { diff, .. } => {
            let diff = diff.read(cx);
            let base_text = diff.base_text().to_string();
            api::ToolCallContent::Diff {
                path: diff.file_path(cx).unwrap_or_default(),
                old_text: (!base_text.is_empty()).then_some(base_text),
                new_text: diff.buffer().read(cx).text(),
            }
        }
        ToolCallContent::Terminal { terminal, .. } => {
            let terminal = terminal.read(cx);
            let command = terminal.command().read(cx).source().to_string();
            let output = match terminal.output() {
                Some(output) => output.content.clone(),
                None => terminal.inner().read(cx).get_content(),
            };
            api::ToolCallContent::Terminal {
                command: (!command.is_empty()).then(|| strip_code_fence(&command)),
                output,
                exit_code: terminal
                    .output()
                    .and_then(|output| output.exit_status.exit_code)
                    .map(|code| code as i32),
            }
        }
        _ => api::ToolCallContent::Markdown {
            markdown: content.to_markdown(cx),
        },
    }
}

fn strip_code_fence(command: &str) -> String {
    command
        .trim()
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim()
        .to_string()
}

fn tool_kind_name(kind: &acp_v2::ToolKind) -> &'static str {
    match kind {
        acp_v2::ToolKind::Read => "read",
        acp_v2::ToolKind::Edit => "edit",
        acp_v2::ToolKind::Delete => "delete",
        acp_v2::ToolKind::Move => "move",
        acp_v2::ToolKind::Search => "search",
        acp_v2::ToolKind::Execute => "execute",
        acp_v2::ToolKind::Think => "think",
        acp_v2::ToolKind::Fetch => "fetch",
        acp_v2::ToolKind::SwitchMode => "switch_mode",
        _ => "other",
    }
}

fn permission_kind_name(kind: acp_v1::PermissionOptionKind) -> &'static str {
    match kind {
        acp_v1::PermissionOptionKind::AllowOnce => "allow_once",
        acp_v1::PermissionOptionKind::AllowAlways => "allow_always",
        acp_v1::PermissionOptionKind::RejectOnce => "reject_once",
        acp_v1::PermissionOptionKind::RejectAlways => "reject_always",
        _ => "other",
    }
}

const CHOICE_OPTION_PREFIX: &str = "choice:";
const FLAT_OPTION_PREFIX: &str = "option:";

/// Desktop shows dropdown-style permissions as a granularity picker plus
/// allow/deny buttons; mobile flattens them into one button per allow choice
/// and a single deny button.
fn permission_options(options: &PermissionOptions) -> Vec<api::PermissionOption> {
    match options {
        PermissionOptions::Flat(options) => options
            .iter()
            .map(|option| api::PermissionOption {
                id: format!("{FLAT_OPTION_PREFIX}{}", option.option_id.0),
                label: option.name.clone(),
                kind: permission_kind_name(option.kind).to_string(),
            })
            .collect(),
        PermissionOptions::Dropdown(choices)
        | PermissionOptions::DropdownWithPatterns { choices, .. } => {
            let mut result = choices
                .iter()
                .enumerate()
                .map(|(index, choice)| api::PermissionOption {
                    id: format!("{CHOICE_OPTION_PREFIX}{index}:allow"),
                    label: choice.allow.name.clone(),
                    kind: permission_kind_name(choice.allow.kind).to_string(),
                })
                .collect::<Vec<_>>();
            let deny_index = choices
                .iter()
                .position(|choice| choice.deny.kind == acp_v1::PermissionOptionKind::RejectOnce)
                .unwrap_or(0);
            if let Some(choice) = choices.get(deny_index) {
                result.push(api::PermissionOption {
                    id: format!("{CHOICE_OPTION_PREFIX}{deny_index}:deny"),
                    label: choice.deny.name.clone(),
                    kind: permission_kind_name(choice.deny.kind).to_string(),
                });
            }
            result
        }
    }
}

pub fn authorize_tool_call(
    conversation: &MobileConversation,
    tool_call_id: &str,
    option_id: &str,
    cx: &mut App,
) -> Result<()> {
    let thread_view = conversation
        .thread_view(cx)
        .context("the thread is still loading")?;
    let thread = thread_view.read(cx).thread.clone();
    let session_id = thread.read(cx).session_id().clone();
    let tool_call_id = acp_v1::ToolCallId::new(tool_call_id.to_string());
    let outcome = {
        let thread = thread.read(cx);
        let (_, tool_call) = thread
            .tool_call(&tool_call_id)
            .context("the tool call no longer exists")?;
        let authorization = tool_call
            .authorization()
            .context("the tool call isn't waiting for permission")?;
        match &authorization.options {
            PermissionOptions::Flat(options) => {
                let option_id = option_id
                    .strip_prefix(FLAT_OPTION_PREFIX)
                    .context("invalid permission option")?;
                let option = options
                    .iter()
                    .find(|option| option.option_id.0.as_ref() == option_id)
                    .context("unknown permission option")?;
                acp_thread::SelectedPermissionOutcome::new(option.option_id.clone(), option.kind)
            }
            PermissionOptions::Dropdown(choices)
            | PermissionOptions::DropdownWithPatterns { choices, .. } => {
                let (index, action) = option_id
                    .strip_prefix(CHOICE_OPTION_PREFIX)
                    .and_then(|rest| rest.split_once(':'))
                    .context("invalid permission option")?;
                let index = index.parse::<usize>()?;
                let choice = choices.get(index).context("unknown permission option")?;
                choice.build_outcome(action == "allow")
            }
        }
    };
    conversation.window.update(cx, |_, window, cx| {
        thread_view.update(cx, |thread_view, cx| {
            thread_view.authorize_tool_call(session_id, tool_call_id, outcome, window, cx);
        });
    })
}

pub fn send_message(conversation: &MobileConversation, text: String, cx: &mut App) -> Result<()> {
    let thread_view = conversation
        .thread_view(cx)
        .context("the thread is still loading")?;
    conversation.window.update(cx, |_, window, cx| {
        thread_view.update(cx, |thread_view, cx| {
            thread_view.send_text(text, window, cx)
        });
    })
}

pub fn cancel_generation(conversation: &MobileConversation, cx: &mut App) -> Result<()> {
    let thread_view = conversation
        .thread_view(cx)
        .context("the thread is still loading")?;
    thread_view.update(cx, |thread_view, cx| thread_view.cancel_generation(cx));
    Ok(())
}

pub fn remove_queued_message(
    conversation: &MobileConversation,
    index: usize,
    cx: &mut App,
) -> Result<()> {
    let thread_view = conversation
        .thread_view(cx)
        .context("the thread is still loading")?;
    thread_view.update(cx, |thread_view, cx| {
        let id = thread_view
            .message_queue
            .iter()
            .nth(index)
            .map(|entry| entry.id)
            .context("the message is no longer queued")?;
        thread_view.remove_from_queue(id, cx);
        cx.notify();
        anyhow::Ok(())
    })
}

fn thread_controls(thread_view: &ThreadView, cx: &App) -> Vec<api::ThreadControl> {
    let mut controls = Vec::new();
    let config_options = thread_view
        .config_options_view
        .as_ref()
        .map(|view| view.read(cx).options())
        .unwrap_or_default();
    let has_config_category = |category: acp_v1::SessionConfigOptionCategory| {
        config_options
            .iter()
            .any(|option| option.category.as_ref() == Some(&category))
    };

    if !has_config_category(acp_v1::SessionConfigOptionCategory::Model)
        && let Some(model_selector) = &thread_view.model_selector
    {
        let delegate = &model_selector.read(cx).selector().read(cx).delegate;
        let mut options = Vec::new();
        let mut push_model = |model: &acp_thread::AgentModelInfo, group: Option<String>| {
            if model.disabled.is_none() {
                options.push(api::ControlOption {
                    value: model.id.to_string(),
                    label: model.name.to_string(),
                    group,
                    description: model
                        .description
                        .as_ref()
                        .map(|description| description.to_string()),
                });
            }
        };
        match delegate.models() {
            Some(AgentModelList::Flat(models)) => {
                for model in models {
                    push_model(model, None);
                }
            }
            Some(AgentModelList::Grouped(groups)) => {
                for (group, models) in groups {
                    for model in models {
                        push_model(model, Some(group.0.to_string()));
                    }
                }
            }
            None => {}
        }
        controls.push(api::ThreadControl {
            id: MODEL_CONTROL.to_string(),
            label: "Model".to_string(),
            kind: api::ThreadControlKind::Select {
                value: delegate.active_model().map(|model| model.id.to_string()),
                options,
            },
        });
    }

    if !has_config_category(acp_v1::SessionConfigOptionCategory::Mode) {
        if let Some(profile_selector) = &thread_view.profile_selector {
            let provider = profile_selector.read(cx).provider();
            if provider.profiles_supported(cx) {
                controls.push(api::ThreadControl {
                    id: PROFILE_CONTROL.to_string(),
                    label: "Profile".to_string(),
                    kind: api::ThreadControlKind::Select {
                        value: Some(provider.profile_id(cx).0.to_string()),
                        options: AgentProfile::available_profiles(cx)
                            .into_iter()
                            .map(|(id, name)| api::ControlOption {
                                value: id.0.to_string(),
                                label: name.to_string(),
                                group: None,
                                description: None,
                            })
                            .collect(),
                    },
                });
            }
        } else if let Some(mode_selector) = &thread_view.mode_selector {
            let mode_selector = mode_selector.read(cx);
            controls.push(api::ThreadControl {
                id: MODE_CONTROL.to_string(),
                label: "Mode".to_string(),
                kind: api::ThreadControlKind::Select {
                    value: Some(mode_selector.mode().0.to_string()),
                    options: mode_selector
                        .all_modes()
                        .into_iter()
                        .map(|mode| api::ControlOption {
                            value: mode.id.0.to_string(),
                            label: mode.name,
                            group: None,
                            description: mode.description,
                        })
                        .collect(),
                },
            });
        }
    }

    for option in &config_options {
        let label = option.name.clone();
        let id = format!("{CONFIG_CONTROL_PREFIX}{}", option.id.0);
        match &option.kind {
            acp_v1::SessionConfigKind::Select(select) => {
                let options = match &select.options {
                    acp_v1::SessionConfigSelectOptions::Ungrouped(options) => options
                        .iter()
                        .map(|value| api::ControlOption {
                            value: value.value.0.to_string(),
                            label: value.name.clone(),
                            group: None,
                            description: value.description.clone(),
                        })
                        .collect(),
                    acp_v1::SessionConfigSelectOptions::Grouped(groups) => groups
                        .iter()
                        .flat_map(|group| {
                            group.options.iter().map(|value| api::ControlOption {
                                value: value.value.0.to_string(),
                                label: value.name.clone(),
                                group: Some(group.name.clone()),
                                description: value.description.clone(),
                            })
                        })
                        .collect(),
                    _ => Vec::new(),
                };
                controls.push(api::ThreadControl {
                    id,
                    label,
                    kind: api::ThreadControlKind::Select {
                        value: Some(select.current_value.0.to_string()),
                        options,
                    },
                });
            }
            acp_v1::SessionConfigKind::Boolean(boolean) => {
                controls.push(api::ThreadControl {
                    id,
                    label,
                    kind: api::ThreadControlKind::Toggle {
                        value: boolean.current_value,
                    },
                });
            }
            _ => {}
        }
    }

    if let Some(native_thread) = thread_view.as_native_thread(cx) {
        let native_thread = native_thread.read(cx);
        if let Some(model) = native_thread.model() {
            if model.supports_thinking() {
                if model.supports_disabling_thinking() {
                    controls.push(api::ThreadControl {
                        id: THINKING_CONTROL.to_string(),
                        label: "Thinking".to_string(),
                        kind: api::ThreadControlKind::Toggle {
                            value: native_thread.thinking_enabled(),
                        },
                    });
                }
                let effort_levels = model.supported_effort_levels();
                if !effort_levels.is_empty() && native_thread.thinking_enabled() {
                    let selected = native_thread.thinking_effort().cloned().or_else(|| {
                        effort_levels
                            .iter()
                            .find(|level| level.is_default)
                            .map(|level| level.value.to_string())
                    });
                    controls.push(api::ThreadControl {
                        id: EFFORT_CONTROL.to_string(),
                        label: "Effort".to_string(),
                        kind: api::ThreadControlKind::Select {
                            value: selected,
                            options: effort_levels
                                .into_iter()
                                .map(|level| api::ControlOption {
                                    value: level.value.to_string(),
                                    label: level.name.to_string(),
                                    group: None,
                                    description: None,
                                })
                                .collect(),
                        },
                    });
                }
            }
        }
        if thread_view.fast_mode_available(cx) {
            controls.push(api::ThreadControl {
                id: FAST_CONTROL.to_string(),
                label: "Fast".to_string(),
                kind: api::ThreadControlKind::Toggle {
                    value: matches!(native_thread.speed(), Some(Speed::Fast)),
                },
            });
        }
    }

    controls
}

pub fn set_control(
    conversation: &MobileConversation,
    control_id: &str,
    value: api::ControlValue,
    cx: &mut App,
) -> Task<Result<()>> {
    let Some(thread_view) = conversation.thread_view(cx) else {
        return Task::ready(Err(anyhow!("the thread is still loading")));
    };
    let select_value = || match &value {
        api::ControlValue::Select(value) => Ok(value.clone()),
        api::ControlValue::Toggle(_) => Err(anyhow!("{control_id} expects a selection")),
    };
    let toggle_value = || match &value {
        api::ControlValue::Toggle(value) => Ok(*value),
        api::ControlValue::Select(_) => Err(anyhow!("{control_id} expects a toggle")),
    };

    let result = (|| -> Result<Option<Task<Result<()>>>> {
        if let Some(config_id) = control_id.strip_prefix(CONFIG_CONTROL_PREFIX) {
            let config_options_view = thread_view
                .read(cx)
                .config_options_view
                .clone()
                .context("the agent has no options")?;
            let config_value = match &value {
                api::ControlValue::Select(value) => acp_v1::SessionConfigOptionValue::value_id(
                    acp_v1::SessionConfigValueId::new(value.clone()),
                ),
                api::ControlValue::Toggle(value) => {
                    acp_v1::SessionConfigOptionValue::boolean(*value)
                }
            };
            let task = config_options_view.update(cx, |view, cx| {
                view.set_option(
                    acp_v1::SessionConfigId::new(config_id.to_string()),
                    config_value,
                    cx,
                )
            });
            return Ok(Some(task));
        }
        match control_id {
            MODEL_CONTROL => {
                let model_id = acp_thread::AgentModelId::new(select_value()?);
                let model_selector = thread_view
                    .read(cx)
                    .model_selector
                    .clone()
                    .context("the agent has no model selector")?;
                let selector = model_selector.read(cx).selector().clone();
                selector.update(cx, |picker, cx| {
                    picker.delegate.select_model_by_id(&model_id, cx)
                })?;
                model_selector.update(cx, |_, cx| cx.notify());
            }
            MODE_CONTROL => {
                let mode_id = acp_v1::SessionModeId::new(select_value()?);
                let mode_selector = thread_view
                    .read(cx)
                    .mode_selector
                    .clone()
                    .context("the agent has no modes")?;
                mode_selector.update(cx, |selector, cx| selector.set_mode(mode_id, cx));
            }
            PROFILE_CONTROL => {
                let profile_id = AgentProfileId(select_value()?.into());
                let profile_selector = thread_view
                    .read(cx)
                    .profile_selector
                    .clone()
                    .context("the agent has no profiles")?;
                let provider = profile_selector.read(cx).provider().clone();
                provider.set_profile(profile_id, cx);
                profile_selector.update(cx, |_, cx| cx.notify());
            }
            THINKING_CONTROL => {
                let enabled = toggle_value()?;
                thread_view.update(cx, |thread_view, cx| {
                    thread_view.apply_thinking_enabled(enabled, cx)
                });
            }
            EFFORT_CONTROL => {
                let effort = select_value()?;
                thread_view.update(cx, |thread_view, cx| {
                    thread_view.apply_thinking_effort(effort, cx)
                });
            }
            FAST_CONTROL => {
                let speed = if toggle_value()? {
                    Speed::Fast
                } else {
                    Speed::Standard
                };
                thread_view.update(cx, |thread_view, cx| {
                    thread_view.apply_fast_mode_speed(speed, cx)
                });
            }
            _ => return Err(anyhow!("unknown control {control_id:?}")),
        }
        thread_view.update(cx, |_, cx| cx.notify());
        Ok(None)
    })();

    match result {
        Ok(Some(task)) => task,
        Ok(None) => Task::ready(Ok(())),
        Err(error) => Task::ready(Err(error)),
    }
}

/// Calls `on_change` whenever the thread view changes in a way that may affect
/// `thread_state` (selectors, queue, errors).
pub fn observe_thread_view(
    conversation: &MobileConversation,
    cx: &mut App,
    on_change: impl Fn(&mut App) + 'static,
) -> Option<gpui::Subscription> {
    let thread_view = conversation.thread_view(cx)?;
    Some(cx.observe(&thread_view, move |_, cx| on_change(cx)))
}

fn tool_call_status(status: ToolCallStatus) -> api::ToolCallStatus {
    match status {
        ToolCallStatus::Pending => api::ToolCallStatus::Pending,
        ToolCallStatus::WaitingForConfirmation => api::ToolCallStatus::WaitingForConfirmation,
        ToolCallStatus::InProgress => api::ToolCallStatus::InProgress,
        ToolCallStatus::Completed => api::ToolCallStatus::Completed,
        ToolCallStatus::Failed => api::ToolCallStatus::Failed,
        ToolCallStatus::Rejected => api::ToolCallStatus::Rejected,
        ToolCallStatus::Canceled => api::ToolCallStatus::Canceled,
    }
}

/// Why the conversation has no thread to work with, if that's the case:
/// the agent failed to start, or it waits for the user to sign in.
pub fn conversation_error(conversation: &MobileConversation, cx: &App) -> Option<String> {
    let conversation_view = conversation.conversation.read(cx);
    conversation_view
        .load_error_message()
        .map(|message| format!("The agent failed to start: {message}"))
        .or_else(|| {
            conversation_view
                .authentication_required_message(cx)
                .map(|message| format!("{message} Sign in from Zed's agent panel."))
        })
}

/// The entries of one run: a user message and everything up to the next one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRange {
    pub run_id: String,
    pub entries: Range<usize>,
}

pub fn run_ranges(conversation: &MobileConversation, cx: &App) -> Vec<RunRange> {
    match conversation.thread(cx) {
        Some(thread) => thread_run_ranges(thread.read(cx)),
        None => Vec::new(),
    }
}

fn thread_run_ranges(thread: &AcpThread) -> Vec<RunRange> {
    let entries = thread.entries();
    // Subagents get their own (indented) user messages, which belong to the
    // run that started them.
    let mut starts = entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| {
            matches!(entry, AgentThreadEntry::UserMessage(message) if !message.indented)
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let has_preamble = starts.first().is_none_or(|first| *first > 0) && !entries.is_empty();
    if has_preamble {
        starts.insert(0, 0);
    }
    let first_number = if has_preamble { 0 } else { 1 };
    starts
        .iter()
        .enumerate()
        .map(|(position, start)| RunRange {
            run_id: (first_number + position).to_string(),
            entries: *start..starts.get(position + 1).copied().unwrap_or(entries.len()),
        })
        .collect()
}

pub fn thread_runs(
    conversation: &MobileConversation,
    include_items: bool,
    cx: &App,
) -> Vec<rest::Run> {
    let ranges = run_ranges(conversation, cx);
    let last_index = ranges.len().saturating_sub(1);
    ranges
        .iter()
        .enumerate()
        .filter_map(|(index, range)| {
            build_run(conversation, range, index == last_index, include_items, cx)
        })
        .collect()
}

pub fn thread_run(
    conversation: &MobileConversation,
    run_id: &str,
    include_items: bool,
    cx: &App,
) -> Option<rest::Run> {
    let ranges = run_ranges(conversation, cx);
    let index = ranges.iter().position(|range| range.run_id == run_id)?;
    build_run(
        conversation,
        &ranges[index],
        index + 1 == ranges.len(),
        include_items,
        cx,
    )
}

pub fn build_run(
    conversation: &MobileConversation,
    range: &RunRange,
    is_last: bool,
    include_items: bool,
    cx: &App,
) -> Option<rest::Run> {
    let thread_view = conversation.thread_view(cx)?;
    let thread_view = thread_view.read(cx);
    let thread = thread_view.thread.read(cx);
    let entries = thread.entries().get(range.entries.clone())?;

    let mut user_message = String::new();
    let mut final_message = None;
    let mut pending = Vec::new();
    let mut message_count = 0;
    let mut tool_call_count = 0;
    let mut has_canceled_tool_call = false;
    for (offset, entry) in entries.iter().enumerate() {
        match entry {
            AgentThreadEntry::UserMessage(message) => {
                if offset == 0 {
                    user_message = message.content.to_markdown(cx);
                }
            }
            AgentThreadEntry::AssistantMessage(message) => {
                message_count += 1;
                if !message.indented && !message.is_subagent_output {
                    let markdown = assistant_markdown(&message.chunks, false, cx);
                    if !markdown.trim().is_empty() {
                        final_message = Some(markdown);
                    }
                }
            }
            AgentThreadEntry::ToolCall(tool_call) => {
                tool_call_count += 1;
                has_canceled_tool_call |= tool_call.status() == ToolCallStatus::Canceled;
                if let Some(authorization) = tool_call.authorization() {
                    pending.push(rest::PendingRequest::Permission {
                        tool_call: run_tool_call(tool_call, cx),
                        options: permission_options(&authorization.options),
                    });
                }
            }
            AgentThreadEntry::Elicitation(id) => {
                if let Some(question) = question(thread, id)
                    && question.status == rest::QuestionStatus::Pending
                {
                    pending.push(rest::PendingRequest::Question(question));
                }
            }
            AgentThreadEntry::ContextCompaction(_) => {}
        }
    }

    let is_generating = is_last && thread.status() == ThreadStatus::Generating;
    let error = is_last.then(|| thread_view.error_message(cx)).flatten();
    let status = if pending
        .iter()
        .any(|request| matches!(request, rest::PendingRequest::Permission { .. }))
    {
        rest::RunStatus::WaitingForPermission
    } else if !pending.is_empty() {
        rest::RunStatus::WaitingForAnswer
    } else if is_generating {
        rest::RunStatus::Running
    } else if error.is_some() {
        rest::RunStatus::Failed
    } else if has_canceled_tool_call {
        rest::RunStatus::Cancelled
    } else {
        rest::RunStatus::Completed
    };

    let items = include_items.then(|| {
        entries
            .iter()
            .map(|entry| run_item(thread, entry, cx))
            .collect()
    });
    Some(rest::Run {
        run_id: range.run_id.clone(),
        status,
        user_message,
        final_message: final_message.filter(|_| status.is_finished()),
        pending,
        error,
        message_count,
        tool_call_count,
        items,
    })
}

/// The item for the thread entry at `index`.
pub fn thread_run_item(
    conversation: &MobileConversation,
    index: usize,
    cx: &App,
) -> Option<rest::RunItem> {
    let thread = conversation.thread(cx)?;
    let thread = thread.read(cx);
    let entry = thread.entries().get(index)?;
    Some(run_item(thread, entry, cx))
}

fn run_item(thread: &AcpThread, entry: &AgentThreadEntry, cx: &App) -> rest::RunItem {
    match entry {
        AgentThreadEntry::UserMessage(message) => rest::RunItem::UserMessage {
            text: message.content.to_markdown(cx),
        },
        AgentThreadEntry::AssistantMessage(message) => {
            let thinking = assistant_markdown(&message.chunks, true, cx);
            rest::RunItem::AssistantMessage {
                markdown: assistant_markdown(&message.chunks, false, cx),
                thinking: (!thinking.trim().is_empty()).then_some(thinking),
                is_subagent: message.indented || message.is_subagent_output,
            }
        }
        AgentThreadEntry::ToolCall(tool_call) => {
            rest::RunItem::ToolCall(run_tool_call(tool_call, cx))
        }
        AgentThreadEntry::Elicitation(id) => match question(thread, id) {
            Some(question) => rest::RunItem::Question(question),
            None => rest::RunItem::AssistantMessage {
                markdown: "The agent asked for input.".to_string(),
                thinking: None,
                is_subagent: false,
            },
        },
        AgentThreadEntry::ContextCompaction(compaction) => {
            let summary = compaction
                .summary
                .iter()
                .map(|block| block.to_markdown(cx).to_string())
                .collect::<Vec<_>>()
                .join("\n\n");
            rest::RunItem::Compaction {
                summary: (!summary.is_empty()).then_some(summary),
            }
        }
    }
}

fn assistant_markdown(chunks: &[AssistantMessageChunk], thoughts: bool, cx: &App) -> String {
    chunks
        .iter()
        .filter_map(|chunk| match chunk {
            AssistantMessageChunk::Message { block, .. } if !thoughts => {
                Some(block.to_markdown(cx))
            }
            AssistantMessageChunk::Thought { block, .. } if thoughts => Some(block.to_markdown(cx)),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Command output is left out; see [`command_output`].
fn run_tool_call(tool_call: &ToolCall, cx: &App) -> rest::RunToolCall {
    let output = tool_call_output(tool_call, cx);
    let is_execute = matches!(tool_call.kind(), acp_v2::ToolKind::Execute);
    let content = tool_call
        .content()
        .iter()
        .filter_map(|content| match content {
            ToolCallContent::Diff(diff) | ToolCallContent::LegacyDiff { diff, .. } => {
                let diff = diff.read(cx);
                let base_text = diff.base_text().to_string();
                Some(rest::RunToolCallContent::Diff {
                    path: diff.file_path(cx).unwrap_or_default(),
                    old_text: (!base_text.is_empty()).then_some(base_text),
                    new_text: diff.buffer().read(cx).text(),
                })
            }
            ToolCallContent::Terminal { .. } => None,
            _ if is_execute => None,
            _ => Some(rest::RunToolCallContent::Markdown {
                markdown: content.to_markdown(cx),
            }),
        })
        .collect();
    rest::RunToolCall {
        tool_call_id: tool_call.id.0.to_string(),
        title: tool_call.label.read(cx).source().to_string(),
        tool_kind: tool_kind_name(tool_call.kind()).to_string(),
        status: tool_call_status(tool_call.status()),
        command: output.command,
        exit_code: output.exit_code,
        output_bytes: output.output.len(),
        content,
        raw_input: tool_call.raw_input.clone(),
    }
}

struct ToolCallOutput {
    command: Option<String>,
    exit_code: Option<i32>,
    output: String,
}

/// What a tool call printed: its terminals' output, and for tools that run
/// commands without a terminal, the text they reported.
fn tool_call_output(tool_call: &ToolCall, cx: &App) -> ToolCallOutput {
    let is_execute = matches!(tool_call.kind(), acp_v2::ToolKind::Execute);
    let mut command = None;
    let mut exit_code = None;
    let mut outputs = Vec::new();
    for content in tool_call.content() {
        match content {
            ToolCallContent::Terminal { terminal, .. } => {
                let terminal = terminal.read(cx);
                let terminal_command = terminal.command().read(cx).source().to_string();
                if command.is_none() && !terminal_command.trim().is_empty() {
                    command = Some(strip_code_fence(&terminal_command));
                }
                exit_code = exit_code.or_else(|| {
                    terminal
                        .output()
                        .and_then(|output| output.exit_status.exit_code)
                        .map(|code| code as i32)
                });
                outputs.push(match terminal.output() {
                    Some(output) => output.content.clone(),
                    None => terminal.inner().read(cx).get_content(),
                });
            }
            ToolCallContent::Diff(_) | ToolCallContent::LegacyDiff { .. } => {}
            _ if is_execute => outputs.push(content.to_markdown(cx).to_string()),
            _ => {}
        }
    }
    if is_execute && command.is_none() {
        command = tool_call
            .raw_input
            .as_ref()
            .and_then(|input| input.get("command"))
            .and_then(|command| command.as_str())
            .map(str::to_string)
            .or_else(|| Some(strip_code_fence(tool_call.label.read(cx).source())));
    }
    ToolCallOutput {
        command,
        exit_code,
        output: outputs.join("\n"),
    }
}

pub fn command_output(
    conversation: &MobileConversation,
    tool_call_id: &str,
    cx: &App,
) -> Result<rest::CommandOutput> {
    let thread = conversation
        .thread(cx)
        .context("the thread is still loading")?;
    let thread = thread.read(cx);
    let (_, tool_call) = thread
        .tool_call(&acp_v1::ToolCallId::new(tool_call_id.to_string()))
        .context("no such tool call")?;
    let output = tool_call_output(tool_call, cx);
    Ok(rest::CommandOutput {
        tool_call_id: tool_call_id.to_string(),
        command: output.command,
        status: tool_call_status(tool_call.status()),
        exit_code: output.exit_code,
        output: output.output,
    })
}

fn question(thread: &AcpThread, id: &ElicitationEntryId) -> Option<rest::Question> {
    let (_, elicitation) = thread.elicitation(id)?;
    let mode = match &elicitation.request.mode {
        acp_v1::ElicitationMode::Form(form) => rest::QuestionMode::Form {
            schema: serde_json::to_value(&form.requested_schema).unwrap_or_default(),
        },
        acp_v1::ElicitationMode::Url(url) => rest::QuestionMode::Url {
            url: url.url.clone(),
        },
        _ => return None,
    };
    Some(rest::Question {
        question_id: id.0.to_string(),
        message: elicitation.request.message.clone(),
        status: match elicitation.status {
            ElicitationStatus::Pending { .. } => rest::QuestionStatus::Pending,
            ElicitationStatus::Accepted => rest::QuestionStatus::Accepted,
            ElicitationStatus::Declined => rest::QuestionStatus::Declined,
            ElicitationStatus::Canceled => rest::QuestionStatus::Cancelled,
            ElicitationStatus::Completed => rest::QuestionStatus::Completed,
        },
        mode,
    })
}

pub fn answer_question(
    conversation: &MobileConversation,
    question_id: &str,
    answer: rest::QuestionResponse,
    cx: &mut App,
) -> Result<()> {
    let thread = conversation
        .thread(cx)
        .context("the thread is still loading")?;
    let id = ElicitationEntryId(question_id.into());
    let (_, elicitation) = thread
        .read(cx)
        .elicitation(&id)
        .context("no such question")?;
    if !matches!(elicitation.status, ElicitationStatus::Pending { .. }) {
        return Err(anyhow!("the question was already answered"));
    }
    let action = match answer.action {
        rest::QuestionAction::Accept => {
            let mut accept = acp_v1::ElicitationAcceptAction::new();
            if !answer.content.is_empty() {
                let content = answer
                    .content
                    .into_iter()
                    .map(|(key, value)| {
                        serde_json::from_value::<acp_v1::ElicitationContentValue>(value)
                            .map(|value| (key.clone(), value))
                            .with_context(|| {
                                format!(
                                    "{key:?} must be a string, number, boolean or list of strings"
                                )
                            })
                    })
                    .collect::<Result<BTreeMap<_, _>>>()?;
                accept = accept.content(content);
            }
            acp_v1::ElicitationAction::Accept(accept)
        }
        rest::QuestionAction::Decline => acp_v1::ElicitationAction::Decline,
        rest::QuestionAction::Cancel => acp_v1::ElicitationAction::Cancel,
    };
    thread.update(cx, |thread, cx| {
        thread.respond_to_elicitation(&id, acp_v1::CreateElicitationResponse::new(action), cx)
    });
    Ok(())
}

/// Resolves a permission answer that names the kind of option rather than its
/// id, then answers it like [`authorize_tool_call`].
pub fn respond_to_permission(
    conversation: &MobileConversation,
    tool_call_id: &str,
    response: rest::PermissionResponse,
    cx: &mut App,
) -> Result<()> {
    let option_id = match (response.option_id, response.kind) {
        (Some(option_id), _) => option_id,
        (None, Some(kind)) => {
            let thread = conversation
                .thread(cx)
                .context("the thread is still loading")?;
            let thread = thread.read(cx);
            let (_, tool_call) = thread
                .tool_call(&acp_v1::ToolCallId::new(tool_call_id.to_string()))
                .context("the tool call no longer exists")?;
            let authorization = tool_call
                .authorization()
                .context("the tool call isn't waiting for permission")?;
            permission_options(&authorization.options)
                .into_iter()
                .find(|option| option.kind == kind)
                .with_context(|| format!("the tool call has no {kind:?} option"))?
                .id
        }
        (None, None) => return Err(anyhow!("either option_id or kind is required")),
    };
    authorize_tool_call(conversation, tool_call_id, &option_id, cx)
}
