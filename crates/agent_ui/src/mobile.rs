//! Exposes agent threads to the mobile web app (`mobile_bridge`): snapshots
//! in the app's wire format, and the actions its thread view can take. Actions
//! go through the same `ThreadView` code paths as Zed's own UI, so the desktop
//! stays in sync.

use acp_thread::{
    AcpThread, AgentModelList, AgentThreadEntry, AssistantMessageChunk, PermissionOptions,
    ThreadStatus, ToolCallContent, ToolCallStatus,
};
use agent_client_protocol::schema::{v1 as acp_v1, v2 as acp_v2};
use agent_settings::{AgentProfile, AgentProfileId};
use anyhow::{Context as _, Result, anyhow};
use gpui::{AnyWindowHandle, App, Entity, Task};
use language_model::Speed;
use mobile_protocol::api;
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
                status: match tool_call.status() {
                    ToolCallStatus::Pending => api::ToolCallStatus::Pending,
                    ToolCallStatus::WaitingForConfirmation => {
                        api::ToolCallStatus::WaitingForConfirmation
                    }
                    ToolCallStatus::InProgress => api::ToolCallStatus::InProgress,
                    ToolCallStatus::Completed => api::ToolCallStatus::Completed,
                    ToolCallStatus::Failed => api::ToolCallStatus::Failed,
                    ToolCallStatus::Rejected => api::ToolCallStatus::Rejected,
                    ToolCallStatus::Canceled => api::ToolCallStatus::Canceled,
                },
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
        thread_view.update(cx, |thread_view, cx| thread_view.send_text(text, window, cx));
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
                    description: model.description.as_ref().map(|description| description.to_string()),
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
                value: delegate
                    .active_model()
                    .map(|model| model.id.to_string()),
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
