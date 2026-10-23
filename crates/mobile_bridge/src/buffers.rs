use anyhow::{Context as _, Result, anyhow, bail, ensure};
use collections::{HashMap, HashSet};
use editor::Editor;
use gpui::{App, AppContext as _, Context, Entity, Subscription, Task, WeakEntity};
use language::{Buffer, BufferEvent, LanguageAwareStyling, language_settings::LanguageSettings};
use mobile_protocol::ClientId;
use project::{
    Project,
    lsp_store::{FormatTrigger, LspFormatTarget},
};
use serde_json::Value;
use std::{cell::RefCell, collections::hash_map::Entry, ops::Range, rc::Rc, time::Duration};
use text::{Bias, OffsetUtf16, Rope, ToOffset as _};
use theme::ActiveTheme as _;

use crate::api::{
    BufferDiagnosticsEvent, BufferHandle, BufferMetadataEvent, BufferReparsedEvent, BufferState,
    BufferUpdatesEvent, ClientUpdate, Completion, Diagnostic, DiagnosticSeverity, EditorTabKind,
    Highlights, Hover, OpenedItem, PushResult, ServerUpdate, WorkspaceId, events,
};
use crate::{MobileBridge, find_workspace, to_json};

/// The `client_id` of updates that originate in Zed rather than on a phone.
const ZED_CLIENT_ID: &str = "zed";
const DIAGNOSTICS_DEBOUNCE: Duration = Duration::from_millis(100);
const REPARSE_DEBOUNCE: Duration = Duration::from_millis(150);

#[derive(Default)]
pub(crate) struct BuffersState {
    entries: HashMap<BufferHandle, BufferEntry>,
    completions: HashMap<(ClientId, BufferHandle), StoredCompletions>,
}

struct BufferEntry {
    buffer: Entity<Buffer>,
    project: WeakEntity<Project>,
    /// The number of updates in the buffer's mobile history so far.
    version: u64,
    /// The buffer version that `version` corresponds to.
    last_synced: clock::Global,
    /// The buffer's text at `last_synced`.
    synced_text: text::BufferSnapshot,
    /// Reference counts, since several views of one client can show the same buffer.
    subscribers: HashMap<ClientId, usize>,
    pending_diagnostics: Option<Task<()>>,
    pending_reparse: Option<Task<()>>,
    _subscription: Subscription,
}

struct StoredCompletions {
    completions: Rc<RefCell<Box<[project::Completion]>>>,
    insert_texts: Vec<String>,
}

/// A replacement of a UTF-16 range of a document.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Utf16Edit {
    range: Range<usize>,
    new_text: String,
}

impl BufferEntry {
    fn new(
        buffer: Entity<Buffer>,
        project: WeakEntity<Project>,
        subscription: Subscription,
        cx: &App,
    ) -> Self {
        let buffer_snapshot = buffer.read(cx).text_snapshot();
        Self {
            buffer,
            project,
            version: 0,
            last_synced: buffer_snapshot.version().clone(),
            synced_text: buffer_snapshot,
            subscribers: HashMap::default(),
            pending_diagnostics: None,
            pending_reparse: None,
            _subscription: subscription,
        }
    }

    fn handle(&self) -> BufferHandle {
        self.buffer.entity_id().as_u64()
    }

    fn mark_synced(&mut self, snapshot: text::BufferSnapshot) {
        self.last_synced = snapshot.version().clone();
        self.synced_text = snapshot;
    }

    /// Records the buffer's edits since the last sync as a single update
    /// attributed to Zed, returning it for the subscribers.
    fn flush(&mut self, cx: &App) -> Option<BufferUpdatesEvent> {
        let snapshot = self.buffer.read(cx).text_snapshot();
        if *snapshot.version() == self.last_synced {
            return None;
        }
        let old_text = self.synced_text.as_rope();
        let edits = snapshot
            .edits_since::<usize>(&self.last_synced)
            .map(|edit| Utf16Edit {
                range: old_text.offset_to_offset_utf16(edit.old.start).0
                    ..old_text.offset_to_offset_utf16(edit.old.end).0,
                new_text: snapshot.text_for_range(edit.new).collect(),
            })
            .collect::<Vec<_>>();
        let old_length = old_text.summary().len_utf16.0;
        self.mark_synced(snapshot);
        if edits.is_empty() {
            return None;
        }

        let event = BufferUpdatesEvent {
            buffer: self.handle(),
            version: self.version,
            updates: vec![ServerUpdate {
                client_id: ZED_CLIENT_ID.to_string(),
                changes: change_set_to_json(&edits, old_length),
            }],
        };
        self.version += 1;
        Some(event)
    }

    /// Applies a client's updates made on top of `version`. The caller must
    /// flush first. Returns the accepted updates (even when a later update
    /// failed to apply) along with the outcome.
    fn apply_updates(
        &mut self,
        client_id: ClientId,
        version: u64,
        updates: Vec<ClientUpdate>,
        cx: &mut App,
    ) -> (Option<BufferUpdatesEvent>, Result<PushResult>) {
        if version != self.version {
            let result = PushResult {
                accepted: false,
                version: self.version,
            };
            return (None, Ok(result));
        }
        if self.buffer.read(cx).read_only() {
            return (None, Err(anyhow!("the buffer is read-only")));
        }

        // Validate every update's structure up front, so that malformed input
        // is rejected before anything is applied.
        let mut document_length = self.synced_text.as_rope().summary().len_utf16.0;
        let mut parsed_updates = Vec::with_capacity(updates.len());
        for update in updates {
            let edits = match parse_change_set(&update.changes, document_length) {
                Ok(edits) => edits,
                Err(error) => return (None, Err(error)),
            };
            document_length = length_after_edits(document_length, &edits);
            parsed_updates.push((update.changes, edits));
        }

        let mut accepted = Vec::with_capacity(parsed_updates.len());
        let mut error = None;
        for (changes, edits) in parsed_updates {
            let edits = match utf16_edits_to_offsets(&edits, self.synced_text.as_rope()) {
                Ok(edits) => edits,
                Err(conversion_error) => {
                    error = Some(conversion_error);
                    break;
                }
            };
            self.buffer.update(cx, |buffer, cx| {
                buffer.edit(edits, None, cx);
            });
            // Recording the edit as synced right away keeps the `Edited` event
            // it emits from turning it into a duplicate update from Zed.
            self.mark_synced(self.buffer.read(cx).text_snapshot());
            self.version += 1;
            accepted.push(ServerUpdate {
                client_id: client_id.to_string(),
                changes,
            });
        }

        let event = (!accepted.is_empty()).then(|| BufferUpdatesEvent {
            buffer: self.handle(),
            version,
            updates: accepted,
        });
        let result = match error {
            Some(error) => Err(error),
            None => Ok(PushResult {
                accepted: true,
                version: self.version,
            }),
        };
        (event, result)
    }
}

impl BuffersState {
    pub fn remove_client(&mut self, client_id: ClientId, _cx: &mut Context<MobileBridge>) {
        self.completions
            .retain(|(completion_client_id, _), _| *completion_client_id != client_id);
        self.entries.retain(|_, entry| {
            entry.subscribers.remove(&client_id);
            !entry.subscribers.is_empty()
        });
    }

    fn entry(&self, handle: BufferHandle) -> Result<&BufferEntry> {
        self.entries
            .get(&handle)
            .context("the buffer isn't open on this device")
    }
}

impl MobileBridge {
    /// Subscribes the client to a buffer's updates. Each call must be balanced
    /// by a `release_buffer`.
    pub(crate) fn subscribe_buffer(
        &mut self,
        client_id: ClientId,
        buffer: Entity<Buffer>,
        project: WeakEntity<Project>,
        cx: &mut Context<Self>,
    ) -> Result<BufferState> {
        let handle = buffer.entity_id().as_u64();
        // Flushing before subscribing keeps the client from receiving updates
        // that precede the state it's about to get.
        self.flush_buffer(handle, cx);
        match self.buffers.entries.entry(handle) {
            Entry::Occupied(mut occupied) => {
                *occupied.get_mut().subscribers.entry(client_id).or_insert(0) += 1;
            }
            Entry::Vacant(vacant) => {
                let subscription = cx.subscribe(&buffer, move |this, _, event, cx| {
                    this.handle_buffer_event(handle, event, cx);
                });
                let mut entry = BufferEntry::new(buffer, project, subscription, cx);
                entry.subscribers.insert(client_id, 1);
                vacant.insert(entry);
            }
        }
        self.buffer_state(handle, cx)
    }

    pub(crate) fn release_buffer(&mut self, client_id: ClientId, handle: BufferHandle) {
        let Some(entry) = self.buffers.entries.get_mut(&handle) else {
            return;
        };
        if let Some(count) = entry.subscribers.get_mut(&client_id) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                entry.subscribers.remove(&client_id);
                self.buffers.completions.remove(&(client_id, handle));
            }
        }
        if entry.subscribers.is_empty() {
            self.buffers.entries.remove(&handle);
        }
    }

    fn buffer_subscribers(&self, handle: BufferHandle) -> Vec<ClientId> {
        self.buffers
            .entries
            .get(&handle)
            .map(|entry| entry.subscribers.keys().copied().collect())
            .unwrap_or_default()
    }

    fn buffer_state(
        &mut self,
        handle: BufferHandle,
        cx: &mut Context<Self>,
    ) -> Result<BufferState> {
        let entry = self.buffers.entry(handle)?;
        let buffer = entry.buffer.clone();
        let has_lsp = entry
            .project
            .upgrade()
            .is_some_and(|project| buffer_has_language_servers(&buffer, &project, cx));

        let entry = self.buffers.entry(handle)?;
        let buffer = entry.buffer.read(cx);
        let settings = LanguageSettings::for_buffer(buffer, cx);
        Ok(BufferState {
            buffer: handle,
            version: entry.version,
            text: entry.synced_text.text(),
            path: buffer_path(buffer),
            language: buffer
                .language()
                .map(|language| language.name().to_string()),
            read_only: buffer.read_only(),
            is_dirty: buffer.is_dirty(),
            has_lsp,
            diagnostics: buffer_diagnostics(&buffer.snapshot()),
            tab_size: settings.tab_size.get(),
            hard_tabs: settings.hard_tabs,
        })
    }

    fn handle_buffer_event(
        &mut self,
        handle: BufferHandle,
        event: &BufferEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            BufferEvent::Edited { .. } | BufferEvent::Reloaded => self.flush_buffer(handle, cx),
            BufferEvent::DirtyChanged
            | BufferEvent::Saved
            | BufferEvent::FileHandleChanged
            | BufferEvent::LanguageChanged(_)
            | BufferEvent::CapabilityChanged => self.send_buffer_metadata(handle, cx),
            BufferEvent::DiagnosticsUpdated => self.schedule_buffer_diagnostics(handle, cx),
            BufferEvent::Reparsed => self.schedule_buffer_reparsed(handle, cx),
            _ => {}
        }
    }

    /// Sends the buffer's edits since the last sync to its subscribers.
    fn flush_buffer(&mut self, handle: BufferHandle, cx: &mut Context<Self>) {
        let Some(entry) = self.buffers.entries.get_mut(&handle) else {
            return;
        };
        if let Some(event) = entry.flush(cx) {
            self.send_buffer_updates(&event);
        }
    }

    fn send_buffer_updates(&self, event: &BufferUpdatesEvent) {
        for client_id in self.buffer_subscribers(event.buffer) {
            self.send_event(client_id, events::BUFFER_UPDATES, event);
        }
    }

    fn send_buffer_metadata(&mut self, handle: BufferHandle, cx: &mut Context<Self>) {
        let Some(entry) = self.buffers.entries.get(&handle) else {
            return;
        };
        let buffer = entry.buffer.read(cx);
        let event = BufferMetadataEvent {
            buffer: handle,
            is_dirty: buffer.is_dirty(),
            language: buffer
                .language()
                .map(|language| language.name().to_string()),
            path: buffer_path(buffer),
        };
        for client_id in self.buffer_subscribers(handle) {
            self.send_event(client_id, events::BUFFER_METADATA, &event);
        }
    }

    fn schedule_buffer_diagnostics(&mut self, handle: BufferHandle, cx: &mut Context<Self>) {
        let Some(entry) = self.buffers.entries.get_mut(&handle) else {
            return;
        };
        if entry.pending_diagnostics.is_some() {
            return;
        }
        entry.pending_diagnostics = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(DIAGNOSTICS_DEBOUNCE).await;
            this.update(cx, |this, cx| this.send_buffer_diagnostics(handle, cx))
                .ok();
        }));
    }

    fn send_buffer_diagnostics(&mut self, handle: BufferHandle, cx: &mut Context<Self>) {
        // Diagnostics are positioned in the current text, so its edits must
        // reach the clients first.
        self.flush_buffer(handle, cx);
        let Some(entry) = self.buffers.entries.get_mut(&handle) else {
            return;
        };
        entry.pending_diagnostics = None;
        let event = BufferDiagnosticsEvent {
            buffer: handle,
            version: entry.version,
            diagnostics: buffer_diagnostics(&entry.buffer.read(cx).snapshot()),
        };
        for client_id in self.buffer_subscribers(handle) {
            self.send_event(client_id, events::BUFFER_DIAGNOSTICS, &event);
        }
    }

    fn schedule_buffer_reparsed(&mut self, handle: BufferHandle, cx: &mut Context<Self>) {
        let Some(entry) = self.buffers.entries.get_mut(&handle) else {
            return;
        };
        if entry.pending_reparse.is_some() {
            return;
        }
        entry.pending_reparse = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(REPARSE_DEBOUNCE).await;
            this.update(cx, |this, _| {
                if let Some(entry) = this.buffers.entries.get_mut(&handle) {
                    entry.pending_reparse = None;
                }
                for client_id in this.buffer_subscribers(handle) {
                    this.send_event(
                        client_id,
                        events::BUFFER_REPARSED,
                        &BufferReparsedEvent { buffer: handle },
                    );
                }
            })
            .ok();
        }));
    }

    pub(crate) fn push_buffer_updates(
        &mut self,
        client_id: ClientId,
        handle: BufferHandle,
        version: u64,
        updates: Vec<ClientUpdate>,
        cx: &mut Context<Self>,
    ) -> Task<Result<serde_json::Value>> {
        // Pending desktop edits must get their own version before the
        // client's updates, so that the version check sees them.
        self.flush_buffer(handle, cx);
        let entry = match self.buffers.entries.get_mut(&handle) {
            Some(entry) if entry.subscribers.contains_key(&client_id) => entry,
            _ => return Task::ready(Err(anyhow!("the buffer isn't open on this device"))),
        };
        let (event, result) = entry.apply_updates(client_id, version, updates, cx);
        // Clients confirm their own updates when they see them echoed back,
        // so the pusher gets them too.
        if let Some(event) = event {
            self.send_buffer_updates(&event);
        }
        Task::ready(result.and_then(to_json))
    }

    pub(crate) fn buffer_highlights(
        &mut self,
        handle: BufferHandle,
        from: usize,
        to: usize,
        cx: &mut Context<Self>,
    ) -> Task<Result<serde_json::Value>> {
        self.flush_buffer(handle, cx);
        let entry = match self.buffers.entry(handle) {
            Ok(entry) => entry,
            Err(error) => return Task::ready(Err(error)),
        };
        let snapshot = entry.buffer.read(cx).snapshot();
        let rope = snapshot.as_rope();
        let start = clip_utf16_offset(rope, from);
        let end = clip_utf16_offset(rope, to.max(from));
        let syntax_theme = cx.theme().syntax();

        let mut tokens: Vec<(usize, usize, usize)> = Vec::new();
        let mut position = rope.offset_to_offset_utf16(start).0;
        let language_aware = LanguageAwareStyling {
            tree_sitter: true,
            diagnostics: false,
        };
        for chunk in snapshot.chunks(start..end, language_aware) {
            let chunk_end = position + chunk.text.encode_utf16().count();
            if let Some(highlight_id) = chunk.syntax_highlight_id {
                let style_index = usize::from(highlight_id);
                if syntax_theme.get(style_index).is_some() {
                    match tokens.last_mut() {
                        Some(last) if last.1 == position && last.2 == style_index => {
                            last.1 = chunk_end;
                        }
                        _ => tokens.push((position, chunk_end, style_index)),
                    }
                }
            }
            position = chunk_end;
        }

        Task::ready(to_json(Highlights {
            version: entry.version,
            tokens,
        }))
    }

    /// Returns the buffer and project of a subscribed buffer, after checking
    /// that `version` is current.
    fn current_buffer(
        &mut self,
        handle: BufferHandle,
        version: u64,
        cx: &mut Context<Self>,
    ) -> Result<(Entity<Buffer>, Entity<Project>)> {
        self.flush_buffer(handle, cx);
        let entry = self.buffers.entry(handle)?;
        ensure!(entry.version == version, "stale");
        let project = entry.project.upgrade().context("the project was closed")?;
        Ok((entry.buffer.clone(), project))
    }

    pub(crate) fn completions(
        &mut self,
        client_id: ClientId,
        handle: BufferHandle,
        version: u64,
        offset: usize,
        cx: &mut Context<Self>,
    ) -> Task<Result<serde_json::Value>> {
        let request = self
            .current_buffer(handle, version, cx)
            .and_then(|(buffer, project)| {
                let offset = utf16_to_offset(buffer.read(cx).as_rope(), offset)?;
                let context = lsp::CompletionContext {
                    trigger_kind: lsp::CompletionTriggerKind::INVOKED,
                    trigger_character: None,
                };
                Ok(project.update(cx, |project, cx| {
                    project.completions(&buffer, offset, context, cx)
                }))
            });
        let request = match request {
            Ok(request) => request,
            Err(error) => return Task::ready(Err(error)),
        };
        cx.spawn(async move |this, cx| {
            let completions = request
                .await?
                .into_iter()
                .flat_map(|response| response.completions)
                .collect::<Vec<_>>();
            this.update(cx, |this, cx| {
                this.store_completions(client_id, handle, version, completions, cx)
            })?
        })
    }

    fn store_completions(
        &mut self,
        client_id: ClientId,
        handle: BufferHandle,
        version: u64,
        completions: Vec<project::Completion>,
        cx: &mut Context<Self>,
    ) -> Result<serde_json::Value> {
        let (buffer, _) = self.current_buffer(handle, version, cx)?;
        let snapshot = buffer.read(cx).text_snapshot();
        let mut results = Vec::with_capacity(completions.len());
        let mut insert_texts = Vec::with_capacity(completions.len());
        for (index, completion) in completions.iter().enumerate() {
            let insert_text = completion_insert_text(completion);
            let start = completion.replace_range.start.to_offset(&snapshot);
            let end = completion.replace_range.end.to_offset(&snapshot);
            results.push(Completion {
                completion_id: index as u64,
                label: completion.label.text.clone(),
                detail: completion
                    .source
                    .lsp_completion(false)
                    .and_then(|lsp_completion| lsp_completion.detail.clone()),
                kind: completion
                    .kind()
                    .map(|kind| format!("{kind:?}").to_lowercase()),
                from: snapshot.offset_to_offset_utf16(start).0,
                to: snapshot.offset_to_offset_utf16(end).0,
                insert_text: insert_text.clone(),
            });
            insert_texts.push(insert_text);
        }
        self.buffers.completions.insert(
            (client_id, handle),
            StoredCompletions {
                completions: Rc::new(RefCell::new(completions.into_boxed_slice())),
                insert_texts,
            },
        );
        to_json(results)
    }

    pub(crate) fn apply_completion(
        &mut self,
        client_id: ClientId,
        handle: BufferHandle,
        completion_id: u64,
        cx: &mut Context<Self>,
    ) -> Task<Result<serde_json::Value>> {
        match self.start_applying_completion(client_id, handle, completion_id, cx) {
            Ok(additional_edits) => cx.background_spawn(async move {
                additional_edits.await?;
                Ok(serde_json::Value::Null)
            }),
            Err(error) => Task::ready(Err(error)),
        }
    }

    fn start_applying_completion(
        &mut self,
        client_id: ClientId,
        handle: BufferHandle,
        completion_id: u64,
        cx: &mut Context<Self>,
    ) -> Result<Task<Result<Option<text::Transaction>>>> {
        self.flush_buffer(handle, cx);
        let entry = self.buffers.entry(handle)?;
        let buffer = entry.buffer.clone();
        let project = entry.project.upgrade().context("the project was closed")?;
        let stored = self
            .buffers
            .completions
            .get(&(client_id, handle))
            .context("no completions were requested for this buffer")?;
        let completion_index = usize::try_from(completion_id)?;
        let completion = stored
            .completions
            .borrow()
            .get(completion_index)
            .cloned()
            .context("unknown completion")?;
        let insert_text = stored
            .insert_texts
            .get(completion_index)
            .cloned()
            .context("unknown completion")?;
        let completions = stored.completions.clone();

        let replace_range = completion.replace_range;
        buffer.update(cx, |buffer, cx| {
            buffer.edit([(replace_range.clone(), insert_text)], None, cx);
        });
        self.flush_buffer(handle, cx);

        let lsp_store = project.read(cx).lsp_store();
        Ok(lsp_store.update(cx, |lsp_store, cx| {
            lsp_store.apply_additional_edits_for_completion(
                buffer,
                completions,
                completion_index,
                true,
                vec![replace_range],
                cx,
            )
        }))
    }

    pub(crate) fn hover(
        &mut self,
        handle: BufferHandle,
        version: u64,
        offset: usize,
        cx: &mut Context<Self>,
    ) -> Task<Result<serde_json::Value>> {
        let request = self
            .current_buffer(handle, version, cx)
            .and_then(|(buffer, project)| {
                let byte_offset = utf16_to_offset(buffer.read(cx).as_rope(), offset)?;
                Ok(project.update(cx, |project, cx| project.hover(&buffer, byte_offset, cx)))
            });
        let request = match request {
            Ok(request) => request,
            Err(error) => return Task::ready(Err(error)),
        };
        cx.spawn(async move |this, cx| {
            let hovers = request.await.unwrap_or_default();
            let markdown = hovers
                .iter()
                .flat_map(|hover| &hover.contents)
                .filter(|block| !block.text.trim().is_empty())
                .map(|block| match &block.kind {
                    project::HoverBlockKind::PlainText | project::HoverBlockKind::Markdown => {
                        block.text.clone()
                    }
                    project::HoverBlockKind::Code { language } => {
                        format!("```{language}\n{}\n```", block.text)
                    }
                })
                .collect::<Vec<_>>()
                .join("\n\n");
            if markdown.is_empty() {
                return Ok(serde_json::Value::Null);
            }
            let range = hovers.iter().find_map(|hover| hover.range.clone());
            this.update(cx, |this, cx| {
                let (from, to) = this
                    .buffers
                    .entries
                    .get(&handle)
                    .filter(|entry| entry.version == version)
                    .zip(range)
                    .map_or((offset, offset), |(entry, range)| {
                        let snapshot = entry.buffer.read(cx).text_snapshot();
                        (
                            snapshot
                                .offset_to_offset_utf16(range.start.to_offset(&snapshot))
                                .0,
                            snapshot
                                .offset_to_offset_utf16(range.end.to_offset(&snapshot))
                                .0,
                        )
                    });
                to_json(Hover { markdown, from, to })
            })?
        })
    }

    pub(crate) fn go_to_definition(
        &mut self,
        workspace_id: WorkspaceId,
        handle: BufferHandle,
        version: u64,
        offset: usize,
        cx: &mut Context<Self>,
    ) -> Task<Result<serde_json::Value>> {
        let request = self
            .current_buffer(handle, version, cx)
            .and_then(|(buffer, project)| {
                let byte_offset = utf16_to_offset(buffer.read(cx).as_rope(), offset)?;
                Ok(project.update(cx, |project, cx| {
                    project.definitions(&buffer, byte_offset, cx)
                }))
            });
        let request = match request {
            Ok(request) => request,
            Err(error) => return Task::ready(Err(error)),
        };
        cx.spawn(async move |_, cx| {
            let Some(link) = request.await?.and_then(|links| links.into_iter().next()) else {
                return Ok(serde_json::Value::Null);
            };
            cx.update(|cx| {
                let (window, workspace) = find_workspace(workspace_id, cx)?;
                let target_buffer = link.target.buffer;
                let target_snapshot = target_buffer.read(cx).text_snapshot();
                let target_start = link.target.range.start.to_offset(&target_snapshot);
                let target_point = target_snapshot.offset_to_point(target_start);
                let target_offset = target_snapshot.offset_to_offset_utf16(target_start).0;
                let editor = window.update(cx, |_, window, cx| {
                    workspace.update(cx, |workspace, cx| {
                        let editor = workspace.open_project_item::<Editor>(
                            None,
                            target_buffer,
                            true,
                            true,
                            false,
                            true,
                            window,
                            cx,
                        );
                        editor.update(cx, |editor, cx| {
                            editor.go_to_singleton_buffer_point(target_point, window, cx);
                        });
                        editor
                    })
                })?;
                to_json(OpenedItem {
                    item_id: editor.entity_id().as_u64(),
                    kind: EditorTabKind::File,
                    offset: Some(target_offset),
                })
            })
        })
    }

    fn buffer_and_project(
        &self,
        handle: BufferHandle,
    ) -> Result<(Entity<Buffer>, Entity<Project>)> {
        let entry = self.buffers.entry(handle)?;
        let project = entry.project.upgrade().context("the project was closed")?;
        Ok((entry.buffer.clone(), project))
    }

    pub(crate) fn format_buffer(
        &mut self,
        handle: BufferHandle,
        cx: &mut Context<Self>,
    ) -> Task<Result<serde_json::Value>> {
        let (buffer, project) = match self.buffer_and_project(handle) {
            Ok(result) => result,
            Err(error) => return Task::ready(Err(error)),
        };
        let format = project.update(cx, |project, cx| {
            project.format(
                HashSet::from_iter([buffer]),
                LspFormatTarget::Buffers,
                true,
                FormatTrigger::Manual,
                cx,
            )
        });
        cx.background_spawn(async move {
            format.await?;
            Ok(serde_json::Value::Null)
        })
    }

    pub(crate) fn save_buffer(
        &mut self,
        handle: BufferHandle,
        cx: &mut Context<Self>,
    ) -> Task<Result<serde_json::Value>> {
        let (buffer, project) = match self.buffer_and_project(handle) {
            Ok(result) => result,
            Err(error) => return Task::ready(Err(error)),
        };
        let save = project.update(cx, |project, cx| project.save_buffer(buffer, cx));
        cx.background_spawn(async move {
            save.await?;
            Ok(serde_json::Value::Null)
        })
    }
}

fn buffer_path(buffer: &Buffer) -> Option<String> {
    buffer
        .file()
        .map(|file| file.path().as_unix_str().to_string())
}

fn buffer_has_language_servers(
    buffer: &Entity<Buffer>,
    project: &Entity<Project>,
    cx: &mut App,
) -> bool {
    let (is_local, lsp_store, languages) = {
        let project = project.read(cx);
        (
            project.is_local(),
            project.lsp_store(),
            project.languages().clone(),
        )
    };
    if is_local {
        lsp_store.update(cx, |lsp_store, cx| {
            buffer.update(cx, |buffer, cx| {
                lsp_store
                    .running_language_servers_for_local_buffer(buffer, cx)
                    .next()
                    .is_some()
            })
        })
    } else {
        // The language servers of remote projects run on the host, so this is
        // the closest available approximation.
        buffer
            .read(cx)
            .language()
            .is_some_and(|language| !languages.lsp_adapters(&language.name()).is_empty())
    }
}

fn buffer_diagnostics(snapshot: &language::BufferSnapshot) -> Vec<Diagnostic> {
    snapshot
        .diagnostics_in_range::<_, usize>(0..snapshot.len(), false)
        .map(|entry| Diagnostic {
            from: snapshot.offset_to_offset_utf16(entry.range.start).0,
            to: snapshot.offset_to_offset_utf16(entry.range.end).0,
            severity: match entry.diagnostic.severity {
                lsp::DiagnosticSeverity::ERROR => DiagnosticSeverity::Error,
                lsp::DiagnosticSeverity::WARNING => DiagnosticSeverity::Warning,
                lsp::DiagnosticSeverity::HINT => DiagnosticSeverity::Hint,
                _ => DiagnosticSeverity::Info,
            },
            message: entry.diagnostic.message.as_str().to_string(),
            source: entry.diagnostic.source.clone(),
        })
        .collect()
}

fn completion_insert_text(completion: &project::Completion) -> String {
    if completion.is_snippet()
        && let Ok(snippet) = snippet::Snippet::parse(&completion.new_text)
    {
        snippet.text
    } else {
        completion.new_text.clone()
    }
}

/// Converts a UTF-16 offset to a byte offset, rejecting offsets that are out
/// of bounds or that split a surrogate pair.
fn utf16_to_offset(text: &Rope, offset: usize) -> Result<usize> {
    let length = text.summary().len_utf16.0;
    ensure!(
        offset <= length,
        "offset {offset} is past the end of the document ({length})"
    );
    let byte_offset = text.offset_utf16_to_offset(OffsetUtf16(offset));
    ensure!(
        text.clip_offset(byte_offset, Bias::Left) == byte_offset
            && text.offset_to_offset_utf16(byte_offset).0 == offset,
        "offset {offset} splits a character"
    );
    Ok(byte_offset)
}

/// Converts a UTF-16 offset to the byte offset of the character boundary at
/// or before it, clamping it to the document.
fn clip_utf16_offset(text: &Rope, offset: usize) -> usize {
    let byte_offset = text.offset_utf16_to_offset(OffsetUtf16(offset));
    text.clip_offset(byte_offset, Bias::Left)
}

fn utf16_edits_to_offsets(edits: &[Utf16Edit], text: &Rope) -> Result<Vec<(Range<usize>, String)>> {
    edits
        .iter()
        .map(|edit| {
            let start = utf16_to_offset(text, edit.range.start)?;
            let end = utf16_to_offset(text, edit.range.end)?;
            Ok((start..end, edit.new_text.clone()))
        })
        .collect()
}

fn length_after_edits(document_length: usize, edits: &[Utf16Edit]) -> usize {
    edits.iter().fold(document_length, |length, edit| {
        length - edit.range.len() + edit.new_text.encode_utf16().count()
    })
}

/// Serializes edits (sorted, non-overlapping and relative to a document of
/// `document_length` UTF-16 code units) in CodeMirror's `ChangeSet.toJSON()`
/// format.
fn change_set_to_json(edits: &[Utf16Edit], document_length: usize) -> Value {
    let mut sections = Vec::with_capacity(edits.len() * 2 + 1);
    let mut position = 0;
    for edit in edits {
        if edit.range.start > position {
            sections.push(Value::from(edit.range.start - position));
        }
        let mut section = vec![Value::from(edit.range.len())];
        if !edit.new_text.is_empty() {
            section.extend(edit.new_text.split('\n').map(Value::from));
        }
        sections.push(Value::Array(section));
        position = position.max(edit.range.end);
    }
    if document_length > position {
        sections.push(Value::from(document_length - position));
    }
    Value::Array(sections)
}

/// Parses CodeMirror's `ChangeSet.toJSON()` format into edits relative to a
/// document of `document_length` UTF-16 code units.
fn parse_change_set(changes: &Value, document_length: usize) -> Result<Vec<Utf16Edit>> {
    let sections = changes
        .as_array()
        .context("a change set must be an array")?;
    let parse_length = |value: &Value| -> Result<usize> {
        let length = value
            .as_u64()
            .with_context(|| format!("invalid change set length: {value}"))?;
        Ok(usize::try_from(length)?)
    };

    let mut edits = Vec::new();
    let mut position = 0usize;
    for section in sections {
        match section {
            Value::Number(_) => {
                position = position
                    .checked_add(parse_length(section)?)
                    .context("change set length overflow")?;
            }
            Value::Array(parts) => {
                let (deleted, lines) = parts.split_first().context("empty change set section")?;
                let end = position
                    .checked_add(parse_length(deleted)?)
                    .context("change set length overflow")?;
                let lines = lines
                    .iter()
                    .map(|line| line.as_str().context("inserted lines must be strings"))
                    .collect::<Result<Vec<_>>>()?;
                let new_text = lines.join("\n");
                if end > position || !new_text.is_empty() {
                    edits.push(Utf16Edit {
                        range: position..end,
                        new_text,
                    });
                }
                position = end;
            }
            _ => bail!("invalid change set section: {section}"),
        }
        if position > document_length {
            return Err(anyhow!(
                "the change set is longer than the document ({document_length})"
            ));
        }
    }
    ensure!(
        position == document_length,
        "the change set covers {position} code units but the document has {document_length}"
    );
    Ok(edits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use serde_json::json;

    fn edit(range: Range<usize>, new_text: &str) -> Utf16Edit {
        Utf16Edit {
            range,
            new_text: new_text.to_string(),
        }
    }

    fn apply_utf16_edits(text: &str, edits: &[Utf16Edit]) -> Result<String> {
        let rope = Rope::from(text);
        let mut result = text.to_string();
        for (range, new_text) in utf16_edits_to_offsets(edits, &rope)?.into_iter().rev() {
            result.replace_range(range, &new_text);
        }
        Ok(result)
    }

    #[test]
    fn test_change_set_round_trip() {
        let edits = vec![edit(2..2, "inserted"), edit(5..8, ""), edit(10..11, "x")];
        let changes = change_set_to_json(&edits, 14);
        assert_eq!(changes, json!([2, [0, "inserted"], 3, [3], 2, [1, "x"], 3]));
        assert_eq!(parse_change_set(&changes, 14).unwrap(), edits);
    }

    #[test]
    fn test_multi_line_insert() {
        let edits = vec![edit(1..1, "one\ntwo\n")];
        let changes = change_set_to_json(&edits, 3);
        assert_eq!(changes, json!([1, [0, "one", "two", ""], 2]));
        assert_eq!(parse_change_set(&changes, 3).unwrap(), edits);
        assert_eq!(
            apply_utf16_edits("abc", &edits).unwrap(),
            "aone\ntwo\nbc".to_string()
        );
    }

    #[test]
    fn test_delete_at_end_of_document() {
        let edits = vec![edit(3..5, "")];
        let changes = change_set_to_json(&edits, 5);
        assert_eq!(changes, json!([3, [2]]));
        assert_eq!(parse_change_set(&changes, 5).unwrap(), edits);
        assert_eq!(apply_utf16_edits("hello", &edits).unwrap(), "hel");
    }

    #[test]
    fn test_empty_change_sets() {
        assert_eq!(change_set_to_json(&[], 0), json!([]));
        assert_eq!(change_set_to_json(&[], 4), json!([4]));
        assert_eq!(parse_change_set(&json!([]), 0).unwrap(), Vec::new());
        assert_eq!(parse_change_set(&json!([4]), 4).unwrap(), Vec::new());
    }

    #[test]
    fn test_surrogate_pairs() {
        // "a😀b": the emoji is two UTF-16 code units and four bytes.
        let text = "a😀b";
        let rope = Rope::from(text);
        assert_eq!(rope.summary().len_utf16.0, 4);
        assert_eq!(utf16_to_offset(&rope, 1).unwrap(), 1);
        assert_eq!(utf16_to_offset(&rope, 3).unwrap(), 5);
        assert!(utf16_to_offset(&rope, 2).is_err());
        assert!(utf16_to_offset(&rope, 5).is_err());

        let replace_emoji = vec![edit(1..3, "é")];
        let changes = change_set_to_json(&replace_emoji, 4);
        assert_eq!(changes, json!([1, [2, "é"], 1]));
        assert_eq!(apply_utf16_edits(text, &replace_emoji).unwrap(), "aéb");
        assert!(apply_utf16_edits(text, &[edit(2..3, "")]).is_err());
    }

    #[test]
    fn test_invalid_change_sets() {
        assert!(parse_change_set(&json!({}), 3).is_err());
        assert!(parse_change_set(&json!([2]), 3).is_err());
        assert!(parse_change_set(&json!([4]), 3).is_err());
        assert!(parse_change_set(&json!([[]]), 0).is_err());
        assert!(parse_change_set(&json!([[1, 2]]), 1).is_err());
        assert!(parse_change_set(&json!([-1]), 0).is_err());
        assert!(parse_change_set(&json!(["text"]), 0).is_err());
    }

    #[test]
    fn test_length_after_edits() {
        let edits = vec![edit(0..2, "😀😀"), edit(3..3, "x")];
        assert_eq!(length_after_edits(5, &edits), 8);
    }

    fn client_update(changes: Value) -> ClientUpdate {
        ClientUpdate { changes }
    }

    #[gpui::test]
    fn test_flush_records_buffer_edits(cx: &mut TestAppContext) {
        let buffer = cx.new(|cx| Buffer::local("hello 😀 world", cx));
        cx.update(|cx| {
            let mut entry = BufferEntry::new(
                buffer.clone(),
                WeakEntity::new_invalid(),
                Subscription::new(|| {}),
                cx,
            );
            assert!(entry.flush(cx).is_none());

            buffer.update(cx, |buffer, cx| {
                buffer.edit([(0..5, "goodbye"), (11..11, "big ")], None, cx);
            });
            let event = entry.flush(cx).unwrap();
            assert_eq!(event.version, 0);
            assert_eq!(event.updates.len(), 1);
            assert_eq!(event.updates[0].client_id, ZED_CLIENT_ID);
            assert_eq!(
                event.updates[0].changes,
                json!([[5, "goodbye"], 4, [0, "big "], 5])
            );
            assert_eq!(entry.version, 1);
            assert!(entry.flush(cx).is_none());

            let reapplied = apply_utf16_edits(
                "hello 😀 world",
                &parse_change_set(&event.updates[0].changes, 14).unwrap(),
            )
            .unwrap();
            assert_eq!(reapplied, buffer.read(cx).text());
        });
    }

    #[gpui::test]
    fn test_push_with_concurrent_local_edit(cx: &mut TestAppContext) {
        let buffer = cx.new(|cx| Buffer::local("one\ntwo", cx));
        cx.update(|cx| {
            let mut entry = BufferEntry::new(
                buffer.clone(),
                WeakEntity::new_invalid(),
                Subscription::new(|| {}),
                cx,
            );

            // The desktop edits while the client's push is in flight.
            buffer.update(cx, |buffer, cx| {
                buffer.edit([(0..0, "zero\n")], None, cx);
            });
            let flushed = entry.flush(cx).unwrap();
            assert_eq!(flushed.version, 0);
            assert_eq!(flushed.updates[0].changes, json!([[0, "zero", ""], 7]));

            // The client's push was based on version 0, so it's rejected.
            let (event, result) =
                entry.apply_updates(7, 0, vec![client_update(json!([3, [0, "!"], 4]))], cx);
            assert!(event.is_none());
            let result = result.unwrap();
            assert!(!result.accepted);
            assert_eq!(result.version, 1);

            // After rebasing onto the desktop's update, it's accepted.
            let (event, result) = entry.apply_updates(
                7,
                1,
                vec![
                    client_update(json!([8, [0, "!"], 4])),
                    client_update(json!([13, [0, "\n😀"]])),
                ],
                cx,
            );
            let result = result.unwrap();
            assert!(result.accepted);
            assert_eq!(result.version, 3);
            let event = event.unwrap();
            assert_eq!(event.version, 1);
            assert_eq!(event.updates.len(), 2);
            assert_eq!(event.updates[0].client_id, "7");
            assert_eq!(event.updates[1].changes, json!([13, [0, "\n😀"]]));
            assert_eq!(buffer.read(cx).text(), "zero\none!\ntwo\n😀");

            // Our own edits don't come back as updates from Zed.
            assert!(entry.flush(cx).is_none());
            assert_eq!(entry.version, 3);

            // Malformed updates are rejected without touching the buffer.
            let (event, result) =
                entry.apply_updates(7, 3, vec![client_update(json!([2, [1]]))], cx);
            assert!(event.is_none());
            assert!(result.is_err());
            assert_eq!(entry.version, 3);

            // An update that splits the emoji is rejected, but the valid
            // update before it is kept and reported.
            let (event, result) = entry.apply_updates(
                7,
                3,
                vec![
                    client_update(json!([[0, "a"], 16])),
                    client_update(json!([16, [1]])),
                ],
                cx,
            );
            assert!(result.is_err());
            assert_eq!(event.unwrap().updates.len(), 1);
            assert_eq!(entry.version, 4);
            assert_eq!(buffer.read(cx).text(), "azero\none!\ntwo\n😀");
        });
    }
}
