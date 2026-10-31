//! Long-running HTTP API requests (creating worktrees and threads), whose
//! progress clients follow as an event stream.

use std::collections::VecDeque;
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use agent_ui::AgentPanel;
use agent_ui::mobile as agent_mobile;
use anyhow::{Context as _, Result, anyhow};
use collections::HashSet;
use futures::StreamExt as _;
use gpui::{App, AsyncApp, Context, Entity, Task, WeakEntity, WindowHandle};
use mobile_protocol::RequestId;
use mobile_protocol::api::WorktreeChoice;
use mobile_protocol::rest::{
    self, OperationEventKind, OperationKind, OperationResult, OperationStatus, OperationStep,
};
use sidebar::mobile::find_or_open_workspace;
use terminal_view::TerminalView;
use util::ResultExt as _;
use util::path_list::PathList;
use workspace::tasks::{WorktreeHookEvent, WorktreeHookEventHandler};
use workspace::{MultiWorkspace, ProjectGroupKey, Workspace};

use crate::MobileBridge;
use crate::rest::{Reply, StreamTarget, not_found, send_message};
use crate::threads::{
    find_project_group, wait_for_agent_panel, wait_for_conversation, workspace_paths,
};
use crate::workspace_items::all_panes;

/// Finished operations beyond this many are forgotten, oldest first.
const MAX_OPERATIONS: usize = 100;
/// How much of a hook's output is reported when it finishes.
const MAX_HOOK_OUTPUT_CHARS: usize = 4000;

#[derive(Default)]
pub(crate) struct OperationsState {
    next_id: u64,
    operations: VecDeque<OperationRecord>,
}

struct OperationRecord {
    operation: rest::Operation,
    streams: HashSet<RequestId>,
    _task: Option<Task<()>>,
}

impl OperationsState {
    pub fn list(&self) -> Vec<rest::Operation> {
        self.operations
            .iter()
            .map(|record| record.operation.clone())
            .collect()
    }

    pub fn get(&self, operation_id: &str) -> Option<rest::Operation> {
        self.find(operation_id)
            .map(|record| record.operation.clone())
    }

    pub fn remove_stream(&mut self, request_id: RequestId, operation_id: &str) {
        if let Some(record) = self.find_mut(operation_id) {
            record.streams.remove(&request_id);
        }
    }

    fn find(&self, operation_id: &str) -> Option<&OperationRecord> {
        self.operations
            .iter()
            .find(|record| record.operation.operation_id == operation_id)
    }

    fn find_mut(&mut self, operation_id: &str) -> Option<&mut OperationRecord> {
        self.operations
            .iter_mut()
            .find(|record| record.operation.operation_id == operation_id)
    }

    fn evict_finished(&mut self) {
        while self.operations.len() > MAX_OPERATIONS {
            let Some(index) = self
                .operations
                .iter()
                .position(|record| record.operation.status != OperationStatus::Running)
            else {
                return;
            };
            self.operations.remove(index);
        }
    }
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

/// Reports an operation's progress from its task.
#[derive(Clone)]
struct Reporter {
    bridge: WeakEntity<MobileBridge>,
    operation_id: String,
}

impl Reporter {
    fn report(&self, kind: OperationEventKind, cx: &mut AsyncApp) {
        self.bridge
            .update(cx, |bridge, _| {
                bridge.report_operation(&self.operation_id, kind)
            })
            .log_err();
    }

    fn step(&self, step: OperationStep, message: &str, cx: &mut AsyncApp) {
        self.report(
            OperationEventKind::Step {
                step,
                message: message.to_string(),
            },
            cx,
        );
    }
}

impl MobileBridge {
    /// Runs `run` as a new operation, answering the request with it right
    /// away.
    fn start_operation(
        &mut self,
        kind: OperationKind,
        cx: &mut Context<Self>,
        run: impl AsyncFnOnce(&Reporter, &mut AsyncApp) -> Result<OperationResult> + 'static,
    ) -> Result<Reply> {
        self.operations.next_id += 1;
        let operation_id = format!("op_{}", self.operations.next_id);
        let operation = rest::Operation {
            operation_id: operation_id.clone(),
            kind,
            status: OperationStatus::Running,
            started_at: unix_millis(),
            finished_at: None,
            events: Vec::new(),
            result: None,
            error: None,
        };
        let reporter = Reporter {
            bridge: cx.weak_entity(),
            operation_id,
        };
        let task = cx.spawn(async move |this, cx| {
            let result = run(&reporter, cx).await;
            let event = match result {
                Ok(result) => OperationEventKind::Succeeded { result },
                Err(error) => OperationEventKind::Failed {
                    error: format!("{error:#}"),
                },
            };
            this.update(cx, |this, _| {
                this.report_operation(&reporter.operation_id, event)
            })
            .log_err();
        });
        self.operations.operations.push_back(OperationRecord {
            operation: operation.clone(),
            streams: HashSet::default(),
            _task: Some(task),
        });
        self.operations.evict_finished();
        Ok(Reply::Json(202, serde_json::to_value(operation)?))
    }

    fn report_operation(&mut self, operation_id: &str, kind: OperationEventKind) {
        let Some(record) = self.operations.find_mut(operation_id) else {
            return;
        };
        let operation = &mut record.operation;
        if operation.status != OperationStatus::Running {
            return;
        }
        match &kind {
            OperationEventKind::Succeeded { result } => {
                operation.status = OperationStatus::Succeeded;
                operation.result = Some(result.clone());
            }
            OperationEventKind::Failed { error } => {
                operation.status = OperationStatus::Failed;
                operation.error = Some(error.clone());
            }
            _ => {}
        }
        let event = rest::OperationEvent {
            sequence: operation.events.len() as u64,
            timestamp: unix_millis(),
            kind,
        };
        let is_finished = operation.status != OperationStatus::Running;
        if is_finished {
            operation.finished_at = Some(event.timestamp);
        }
        operation.events.push(event.clone());
        let streams = if is_finished {
            std::mem::take(&mut record.streams)
        } else {
            record.streams.clone()
        };
        for request_id in streams {
            self.send_stream_event(request_id, event.kind.name(), &event);
            if is_finished {
                self.end_stream(request_id);
            }
        }
    }

    /// Streams the operation's events, starting with those that already
    /// happened.
    pub(crate) fn subscribe_operation_stream(
        &mut self,
        request_id: RequestId,
        operation_id: &str,
    ) -> Result<()> {
        let record = self
            .operations
            .find_mut(operation_id)
            .ok_or_else(|| not_found(format!("no operation {operation_id:?}")))?;
        let events = record.operation.events.clone();
        let is_running = record.operation.status == OperationStatus::Running;
        if is_running {
            record.streams.insert(request_id);
            self.rest.streams.insert(
                request_id,
                StreamTarget::Operation(operation_id.to_string()),
            );
        }
        self.start_stream(request_id);
        for event in &events {
            self.send_stream_event(request_id, event.kind.name(), event);
        }
        if !is_running {
            self.end_stream(request_id);
        }
        Ok(())
    }

    pub(crate) fn start_create_worktree(
        &mut self,
        request: rest::CreateWorktreeRequest,
        cx: &mut Context<Self>,
    ) -> Result<Reply> {
        let (window, key, workspaces) = find_project_group(&request.project_id, cx)
            .ok_or_else(|| not_found(format!("no open project {:?}", request.project_id)))?;
        self.start_operation(
            OperationKind::CreateWorktree,
            cx,
            async move |reporter, cx| {
                let (_, worktree) = create_worktree(
                    reporter,
                    window,
                    key,
                    workspaces,
                    request.name,
                    request.branch,
                    cx,
                )
                .await?;
                Ok(OperationResult::Worktree(worktree))
            },
        )
    }

    pub(crate) fn start_create_thread(
        &mut self,
        request: rest::CreateThreadRequest,
        cx: &mut Context<Self>,
    ) -> Result<Reply> {
        let (window, key, workspaces) = find_project_group(&request.project_id, cx)
            .ok_or_else(|| not_found(format!("no open project {:?}", request.project_id)))?;
        self.start_operation(
            OperationKind::CreateThread,
            cx,
            async move |reporter, cx| {
                let thread = create_thread(reporter, request, window, key, workspaces, cx).await?;
                Ok(OperationResult::Thread(thread))
            },
        )
    }
}

async fn create_worktree(
    reporter: &Reporter,
    window: WindowHandle<MultiWorkspace>,
    key: ProjectGroupKey,
    workspaces: Vec<Entity<Workspace>>,
    name: Option<String>,
    branch: Option<String>,
    cx: &mut AsyncApp,
) -> Result<(Entity<Workspace>, rest::CreatedWorktree)> {
    let source = match workspaces.first() {
        Some(workspace) => workspace.clone(),
        None => {
            reporter.step(OperationStep::OpeningProject, "Opening the project", cx);
            cx.update(|cx| find_or_open_workspace(window, key.clone(), key.path_list().clone(), cx))
                .await?
        }
    };

    reporter.step(
        OperationStep::CreatingWorktree,
        "Creating the git worktree",
        cx,
    );
    let action = zed_actions::CreateWorktree {
        worktree_name: name.filter(|name| !name.trim().is_empty()),
        branch_target: match branch {
            Some(name) => zed_actions::NewWorktreeBranchTarget::ExistingBranch { name },
            None => zed_actions::NewWorktreeBranchTarget::CurrentBranch,
        },
    };
    let (hook_sender, mut hook_events) = futures::channel::mpsc::unbounded();
    let on_hook_event: WorktreeHookEventHandler = Rc::new(move |event, _| {
        if hook_sender.unbounded_send(event).is_err() {
            log::debug!("a worktree hook finished after its operation ended");
        }
    });
    let creation = window.update(cx, |_, window, cx| {
        source.update(cx, |workspace, cx| {
            git_ui_core::worktree_service::create_worktree_workspace_with_hook_events(
                workspace,
                &action,
                window,
                None,
                Some(on_hook_event),
                cx,
            )
        })
    })?;
    let created = creation.await.context("failed to create the worktree")?;
    let workspace = created.workspace.clone();
    let worktree = cx.update(|cx| rest::CreatedWorktree {
        paths: workspace_paths(&workspace, cx),
        branch: workspace
            .read(cx)
            .project()
            .read(cx)
            .active_repository(cx)
            .and_then(|repository| {
                repository
                    .read(cx)
                    .snapshot()
                    .branch
                    .as_ref()
                    .map(|branch| branch.name().to_string())
            }),
    });
    reporter.report(
        OperationEventKind::WorktreeCreated {
            paths: worktree.paths.clone(),
            branch: worktree.branch.clone(),
        },
        cx,
    );

    // The worktree is only ready once its setup hooks finished.
    reporter.step(
        OperationStep::RunningHooks,
        "Running the worktree's create_worktree hooks",
        cx,
    );
    let mut setup_hooks = created.setup_hooks;
    let hooks_result = loop {
        futures::select_biased! {
            event = hook_events.next() => {
                if let Some(event) = event {
                    report_hook_event(reporter, &workspace, event, cx);
                }
            }
            result = setup_hooks => break result,
        }
    };
    // Every hook reports finishing before the hooks' result is sent.
    while let Ok(event) = hook_events.try_recv() {
        report_hook_event(reporter, &workspace, event, cx);
    }
    let paths = worktree.paths.join(", ");
    match hooks_result {
        Ok(Ok(())) => Ok((workspace, worktree)),
        Ok(Err(error)) => Err(error.context(format!(
            "the worktree was created at {paths}, but its setup failed"
        ))),
        Err(_) => Err(anyhow!(
            "the worktree was created at {paths}, but its setup hooks were cancelled"
        )),
    }
}

fn report_hook_event(
    reporter: &Reporter,
    workspace: &Entity<Workspace>,
    event: WorktreeHookEvent,
    cx: &mut AsyncApp,
) {
    let kind = match event {
        WorktreeHookEvent::Started(task) => OperationEventKind::HookStarted {
            label: task.label,
            command: task.command_label,
            cwd: task.cwd.map(|cwd| cwd.to_string_lossy().into_owned()),
        },
        WorktreeHookEvent::Finished { task, error } => OperationEventKind::HookFinished {
            output: cx.update(|cx| hook_output(workspace, &task.id, cx)),
            label: task.label,
            success: error.is_none(),
            error,
        },
    };
    reporter.report(kind, cx);
}

/// The end of what the hook printed, if its terminal is still open.
fn hook_output(workspace: &Entity<Workspace>, task_id: &task::TaskId, cx: &App) -> Option<String> {
    let content = all_panes(workspace, cx).iter().find_map(|pane| {
        pane.read(cx).items().find_map(|item| {
            let terminal = item.downcast::<TerminalView>()?.read(cx).terminal().clone();
            let terminal = terminal.read(cx);
            (&terminal.task()?.spawned_task.id == task_id).then(|| terminal.get_content())
        })
    })?;
    let content = content.trim_end();
    let skip = content
        .chars()
        .count()
        .saturating_sub(MAX_HOOK_OUTPUT_CHARS);
    Some(content.chars().skip(skip).collect())
}

async fn create_thread(
    reporter: &Reporter,
    request: rest::CreateThreadRequest,
    window: WindowHandle<MultiWorkspace>,
    key: ProjectGroupKey,
    workspaces: Vec<Entity<Workspace>>,
    cx: &mut AsyncApp,
) -> Result<rest::CreatedThread> {
    let workspace = match request.worktree {
        WorktreeChoice::Existing { path } => {
            reporter.step(OperationStep::OpeningProject, "Opening the project", cx);
            // Projects without git have no worktrees to pick from, so an
            // empty path stands for the project itself.
            let folder_paths = if key.path_list().paths().len() > 1 || path.is_empty() {
                key.path_list().clone()
            } else {
                PathList::new(&[std::path::PathBuf::from(&path)])
            };
            cx.update(|cx| find_or_open_workspace(window, key.clone(), folder_paths, cx))
                .await?
        }
        WorktreeChoice::New { name, branch } => {
            create_worktree(reporter, window, key, workspaces, name, branch, cx)
                .await?
                .0
        }
    };

    reporter.step(OperationStep::StartingAgent, "Starting the agent", cx);
    wait_for_agent_panel(&workspace, cx).await?;
    let agent_id = match request.agent_id {
        Some(agent_id) => agent_id,
        None => cx.update(|cx| {
            workspace
                .read(cx)
                .panel::<AgentPanel>(cx)
                .map(|panel| panel.read(cx).selected_agent(cx).id().0.to_string())
                .unwrap_or_else(|| agent::ZED_AGENT_ID.to_string())
        }),
    };
    let thread_id =
        cx.update(|cx| agent_mobile::create_thread(&workspace, window.into(), &agent_id, cx))?;
    let thread_key = thread_id.to_key_string();
    reporter.report(
        OperationEventKind::ThreadCreated {
            thread_id: thread_key.clone(),
        },
        cx,
    );
    let conversation = wait_for_conversation(thread_id, cx).await?;

    for (control_id, value) in request.controls {
        let task = cx.update(|cx| agent_mobile::set_control(&conversation, &control_id, value, cx));
        task.await
            .with_context(|| format!("failed to set {control_id:?}"))?;
        reporter.report(OperationEventKind::ControlSet { control_id }, cx);
    }

    let mut run_id = None;
    if let Some(message) = request.message.filter(|message| !message.trim().is_empty()) {
        reporter.step(OperationStep::SendingMessage, "Sending the message", cx);
        let sent = cx.update(|cx| send_message(&conversation, message, cx))?;
        reporter.report(
            OperationEventKind::MessageSent {
                run_id: sent.run_id.clone(),
            },
            cx,
        );
        run_id = sent.run_id;
    }

    Ok(rest::CreatedThread {
        thread_id: thread_key,
        workspace_paths: cx.update(|cx| workspace_paths(&workspace, cx)),
        run_id,
    })
}
