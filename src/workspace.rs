//! Main Session window.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use crate::grid::ResultTableDelegate;
use gpui::{
    App, ClipboardItem, Context, DragMoveEvent, Empty, Entity, FocusHandle, Focusable,
    SharedString, Window, canvas, div, prelude::*, px, uniform_list,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Editor, EditorState, Input, InputEvent, InputState};
use gpui_component::menu::{DropdownMenu, PopupMenuItem};
use gpui_component::scroll::ScrollableElement;
use gpui_component::slider::{Slider, SliderEvent, SliderState};
use gpui_component::status_bar::StatusBar;
use gpui_component::table::{DataTable, TableState};
use gpui_component::{ActiveTheme, Sizable, StyledExt, Theme, ThemeMode, TitleBar, h_flex, v_flex};

use crate::actions::{AutoResizeColumns, OpenSettings};
use crate::ansi::{looks_like_ansi, parse_ansi_lines};
use crate::engine::{EngineEvent, EngineHandle, EngineSnapshot, ParseReport, result_cap};
use crate::highlight::{highlight_lines, nu_highlighter_factory};
use crate::result::{
    Cell, CopyFormat, InvocationResult, ResultBody, ResultId, ResultStatus, ResultStore, TableView,
    copy_result,
};
use crate::settings::UiSettings;

pub struct Workspace {
    engine: EngineHandle,
    events: Receiver<EngineEvent>,
    repl: Entity<EditorState>,
    results: Vec<InvocationResult>,
    next_id: u64,
    history: Vec<String>,
    parse: Option<ParseReport>,
    nu_version: String,
    cwd: String,
    busy: bool,
    help_open: bool,
    history_open: bool,
    variables_open: bool,
    last_exit: Option<i64>,
    last_duration: Option<Duration>,
    snapshot: EngineSnapshot,
    table_filter: Entity<InputState>,
    result_table: Entity<TableState<ResultTableDelegate>>,
    nested_table: Entity<TableState<ResultTableDelegate>>,
    text_lines: HashMap<u64, Arc<Vec<SharedString>>>,
    /// Nested cells expanded inline under their parent row. Keys: `{path}/{row}/{col}`.
    inline_expand: HashSet<String>,
    /// Manual column widths for custom inspect/pairs tables, keyed by table path.
    col_widths: HashMap<String, Vec<f32>>,
    /// Left edge of a custom table (`path`) or a virtual cell (`virtual:{col}`).
    table_origin_x: HashMap<String, f32>,
    /// Viewport width of a visible table, keyed by path (`root`, `sheet`, `virtual`).
    table_viewport_w: HashMap<String, f32>,
    help_commands: ResultStore,
    help_filter: Entity<InputState>,
    variables: ResultStore,
    var_expand: HashSet<String>,
    settings: UiSettings,
    settings_open: bool,
    repl_font_slider: Entity<SliderState>,
    table_font_slider: Entity<SliderState>,
    copy_status: String,
    focus_handle: FocusHandle,
}

impl Workspace {
    pub fn new(
        engine: EngineHandle,
        events: Receiver<EngineEvent>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let settings = UiSettings::load();
        let repl = cx.new(|cx| {
            let mut state = EditorState::new(window, cx)
                .placeholder("Type a Nushell pipeline…")
                .line_number(false)
                .submit_on_enter(true)
                .language("nushell");
            state.set_highlighter_factory(nu_highlighter_factory(), cx);
            state
        });

        window.focus(&repl.read(cx).focus_handle(cx), cx);

        let table_filter =
            cx.new(|cx| InputState::new(window, cx).placeholder("Filter visible rows…"));
        let help_filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter commands…"));
        let repl_font_slider = cx.new(|_| {
            SliderState::new()
                .min(10.0)
                .max(24.0)
                .step(1.0)
                .default_value(settings.repl_font_size)
        });
        let table_font_slider = cx.new(|_| {
            SliderState::new()
                .min(10.0)
                .max(24.0)
                .step(1.0)
                .default_value(settings.table_font_size)
        });
        let workspace = cx.weak_entity();
        let result_table = cx.new(|cx| {
            TableState::new(ResultTableDelegate::empty(workspace.clone()), window, cx)
                .col_resizable(true)
                .sortable(true)
                .row_selectable(true)
                .cell_selectable(false)
        });
        let nested_table = cx.new(|cx| {
            TableState::new(
                ResultTableDelegate {
                    inspect: true,
                    ..ResultTableDelegate::empty(workspace)
                },
                window,
                cx,
            )
            .col_resizable(true)
            .sortable(true)
            .row_selectable(true)
            .cell_selectable(false)
        });
        cx.subscribe(&table_filter, |this, input, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                let text = input.read(cx).value().to_string();
                if let Some(result) = this.results.iter_mut().rev().find(|r| r.expanded) {
                    result.view.search = text;
                    this.refresh_result_table(cx);
                }
                cx.notify();
            }
        })
        .detach();
        cx.subscribe(&help_filter, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                let _ = this;
                cx.notify();
            }
        })
        .detach();

        cx.subscribe_in(
            &repl,
            window,
            |this, editor, event, window, cx| match event {
                InputEvent::Change => {
                    let source = editor.read(cx).value().to_string();
                    this.engine.parse(source);
                }
                InputEvent::PressEnter { secondary, shift } => {
                    this.on_repl_enter(*secondary, *shift, window, cx);
                }
                _ => {}
            },
        )
        .detach();

        cx.subscribe(
            &repl_font_slider,
            |this, slider, event: &SliderEvent, cx| {
                let (SliderEvent::Change(value) | SliderEvent::Release(value)) = event;
                this.settings.set_repl_font(value.start());
                let _ = slider;
                cx.notify();
            },
        )
        .detach();
        cx.subscribe(
            &table_font_slider,
            |this, slider, event: &SliderEvent, cx| {
                let (SliderEvent::Change(value) | SliderEvent::Release(value)) = event;
                this.settings.set_table_font(value.start());
                this.apply_table_font(cx);
                let _ = slider;
                cx.notify();
            },
        )
        .detach();

        poll_engine(cx);

        Self {
            engine,
            events,
            repl,
            results: Vec::new(),
            next_id: 1,
            history: Vec::new(),
            parse: None,
            nu_version: "…".into(),
            cwd: std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "?".into()),
            busy: false,
            help_open: true,
            history_open: false,
            variables_open: false,
            last_exit: None,
            last_duration: None,
            snapshot: EngineSnapshot::default(),
            table_filter,
            result_table,
            nested_table,
            text_lines: HashMap::new(),
            inline_expand: HashSet::new(),
            col_widths: HashMap::new(),
            table_origin_x: HashMap::new(),
            table_viewport_w: HashMap::new(),
            help_commands: ResultStore::default(),
            help_filter,
            variables: ResultStore::default(),
            var_expand: HashSet::new(),
            settings,
            settings_open: false,
            repl_font_slider,
            table_font_slider,
            copy_status: String::new(),
            focus_handle: cx.focus_handle(),
        }
    }

    fn refresh_result_table(&mut self, cx: &mut Context<Self>) {
        let Some(result) = self.results.iter().rev().find(|r| r.expanded) else {
            return;
        };
        let ResultBody::Table(store) = &result.body else {
            return;
        };
        let store = store.clone();
        let view = result.view.clone();
        self.result_table.update(cx, |table, cx| {
            table.delegate_mut().bind(store, view);
            table
                .delegate_mut()
                .set_font_px(self.settings.table_font_size);
            table.refresh(cx);
            cx.notify();
        });
    }

    fn apply_table_font(&mut self, cx: &mut Context<Self>) {
        let size = self.settings.table_font_size;
        self.result_table.update(cx, |table, cx| {
            table.delegate_mut().set_font_px(size);
            table.refresh(cx);
            cx.notify();
        });
        self.nested_table.update(cx, |table, cx| {
            table.delegate_mut().set_font_px(size);
            table.refresh(cx);
            cx.notify();
        });
    }

    fn reset_table_chrome(&mut self) {
        self.inline_expand.clear();
        self.col_widths.clear();
        self.table_origin_x.clear();
        self.table_viewport_w.clear();
    }

    pub(crate) fn toggle_inline_expand(&mut self, key: String, cx: &mut Context<Self>) {
        if self.inline_expand.contains(&key) {
            self.inline_expand.remove(&key);
            let prefix = format!("{key}/");
            self.inline_expand
                .retain(|existing| !existing.starts_with(&prefix));
            self.col_widths
                .retain(|path, _| path != &key && !path.starts_with(&prefix));
        } else {
            self.inline_expand.insert(key.clone());
            self.scroll_expanded_row_into_view(&key, cx);
            self.bind_nested_if_virtual(&key, cx);
        }
        cx.notify();
    }

    fn cache_text_lines(&mut self, id: ResultId, text: &str) {
        if self.text_lines.contains_key(&id.0) {
            return;
        }
        let lines: Vec<SharedString> = text.lines().map(SharedString::from).collect();
        self.text_lines.insert(id.0, Arc::new(lines));
    }

    fn bind_nested_if_virtual(&mut self, key: &str, cx: &mut Context<Self>) {
        let Some(result) = self.results.iter().rev().find(|r| r.expanded) else {
            return;
        };
        let ResultBody::Table(root) = &result.body else {
            return;
        };
        let Some(store) = nested_store(root, key) else {
            return;
        };
        if !store.prefers_virtual() {
            return;
        }
        let store = store.clone();
        let cols = store.columns.len();
        let prefix = key.to_string();
        let font = self.settings.table_font_size;
        self.nested_table.update(cx, |table, cx| {
            table.delegate_mut().expand_prefix = prefix;
            table
                .delegate_mut()
                .bind(store, TableView::for_columns(cols));
            table.delegate_mut().set_font_px(font);
            table.refresh(cx);
            cx.notify();
        });
    }

    pub(crate) fn is_inline_expanded(&self, key: &str) -> bool {
        self.inline_expand.contains(key)
    }

    fn scroll_expanded_row_into_view(&mut self, key: &str, cx: &mut Context<Self>) {
        let Some(store_row) = parse_root_row(key) else {
            return;
        };
        self.result_table.update(cx, |table, cx| {
            if let Some(vis) = table
                .delegate()
                .visible
                .iter()
                .position(|&row| row == store_row)
            {
                table.scroll_to_row(vis, cx);
            }
        });
    }

    fn resize_inspect_column(
        &mut self,
        path: &str,
        col: usize,
        width: f32,
        cx: &mut Context<Self>,
    ) {
        let widths = self.col_widths.entry(path.to_string()).or_default();
        if widths.len() <= col {
            widths.resize(col + 1, 0.0);
        }
        widths[col] = width.clamp(72.0, 2400.0);
        cx.notify();
    }

    pub(crate) fn note_table_origin(&mut self, key: &str, x: f32) {
        self.table_origin_x.insert(key.to_string(), x);
    }

    pub(crate) fn drag_resize_column(
        &mut self,
        spec: &ResizeInspectCol,
        mouse_x: f32,
        cx: &mut Context<Self>,
    ) {
        if spec.path == "virtual" {
            let key = format!("virtual:{}", spec.col);
            let left = self.table_origin_x.get(&key).copied().unwrap_or(mouse_x);
            self.resize_virtual_column(spec.col, mouse_x - left, cx);
            return;
        }
        let left = self.table_origin_x.get(&spec.path).copied().unwrap_or(0.0) + spec.left_offset;
        self.resize_inspect_column(&spec.path, spec.col, mouse_x - left, cx);
    }

    fn resize_virtual_column(&mut self, data_col: usize, width: f32, cx: &mut Context<Self>) {
        let width = width.clamp(72.0, 2400.0);
        self.result_table.update(cx, |table, cx| {
            if let Some(col) = table.delegate_mut().columns.get_mut(data_col + 1) {
                col.width = px(width);
            }
            table.refresh(cx);
            cx.notify();
        });
    }

    fn auto_resize_visible_columns(&mut self, cx: &mut Context<Self>) {
        let Some(result) = self.results.iter().rev().find(|r| r.expanded) else {
            return;
        };
        let ResultBody::Table(root) = &result.body else {
            return;
        };
        let root = root.clone();
        if root.prefers_virtual() {
            self.fit_virtual_table(cx);
            let expanded: Vec<String> = self.inline_expand.iter().cloned().collect();
            for key in expanded {
                if let Some(nested) = nested_store(&root, &key) {
                    let nested = nested.clone();
                    self.fit_custom_table(&key, &nested);
                }
            }
        } else if root.is_pairs() || root.has_nested() {
            self.fit_custom_table("root", &root);
            let expanded: Vec<String> = self.inline_expand.iter().cloned().collect();
            for key in expanded {
                if let Some(nested) = nested_store(&root, &key) {
                    let nested = nested.clone();
                    self.fit_custom_table(&key, &nested);
                }
            }
        } else {
            self.fit_virtual_table(cx);
        }
        cx.notify();
    }

    fn fit_custom_table(&mut self, path: &str, store: &ResultStore) {
        let viewport = self.table_viewport_w.get(path).copied().unwrap_or(0.0);
        if viewport < 80.0 {
            return;
        }
        let available = (viewport - INDEX_COL_PX - SCROLLBAR_PX).max(72.0);
        let preferred: Vec<f32> = (0..store.columns.len())
            .map(|ix| store.column_px(ix))
            .collect();
        let filled = fill_column_widths(&store.columns, &preferred, available);
        self.col_widths.insert(path.to_string(), filled);
    }

    fn fit_virtual_table(&mut self, cx: &mut Context<Self>) {
        let viewport = self.table_viewport_w.get("virtual").copied().unwrap_or(0.0);
        if viewport < 80.0 {
            return;
        }
        let available = (viewport - INDEX_COL_PX - SCROLLBAR_PX).max(72.0);
        self.result_table.update(cx, |table, cx| {
            let store = &table.delegate().store;
            if store.columns.is_empty() {
                return;
            }
            let preferred: Vec<f32> = (0..store.columns.len())
                .map(|ix| store.column_px(ix))
                .collect();
            let filled = fill_column_widths(&store.columns, &preferred, available);
            for (ix, width) in filled.iter().enumerate() {
                if let Some(col) = table.delegate_mut().columns.get_mut(ix + 1) {
                    col.width = px(*width);
                }
            }
            table.refresh(cx);
            cx.notify();
        });
    }

    fn inspect_column_widths(&self, path: &str, store: &ResultStore) -> Vec<f32> {
        let defaults: Vec<f32> = store
            .columns
            .iter()
            .enumerate()
            .map(|(ix, name)| inspect_column_width(name, store.column_px(ix)))
            .collect();
        merge_column_widths(self.col_widths.get(path), &defaults)
    }

    fn drain_events(&mut self, cx: &mut Context<Self>) {
        while let Ok(event) = self.events.try_recv() {
            match event {
                EngineEvent::Ready(snapshot) => {
                    self.nu_version = snapshot.nu_version.clone();
                    self.snapshot = snapshot;
                    self.engine.scope_commands();
                    self.engine.scope_variables();
                }
                EngineEvent::Parse(report) => {
                    self.parse = Some(report);
                }
                EngineEvent::ScopeCommands(store) => {
                    self.help_commands = store;
                }
                EngineEvent::ScopeVariables(store) => {
                    self.variables = store;
                }
                EngineEvent::Eval(report) => {
                    self.busy = false;
                    self.last_exit = Some(report.exit_code);
                    self.last_duration = Some(report.duration);
                    let text_cache =
                        if let Some(result) = self.results.iter_mut().find(|r| r.id == report.id) {
                            result.status = report.status;
                            result.duration = Some(report.duration);
                            result.exit_code = Some(report.exit_code);
                            result.body = report.body;
                            if let ResultBody::Table(store) = &result.body {
                                result.view = TableView::for_columns(store.columns.len());
                            }
                            match &result.body {
                                ResultBody::Text(text)
                                | ResultBody::Diagnostic(text)
                                | ResultBody::Scalar(text)
                                    if text.contains('\n') =>
                                {
                                    Some((result.id, text.clone()))
                                }
                                _ => None,
                            }
                        } else {
                            None
                        };
                    if let Some((id, text)) = text_cache {
                        self.cache_text_lines(id, &text);
                    }
                    self.refresh_result_table(cx);
                    if self.variables_open {
                        self.engine.scope_variables();
                    }
                }
                EngineEvent::Failed(msg) => {
                    self.busy = false;
                    self.results.push(InvocationResult {
                        id: ResultId(self.next_id),
                        source: String::new(),
                        status: ResultStatus::Failed,
                        duration: None,
                        exit_code: Some(1),
                        body: ResultBody::Diagnostic(msg),
                        log: Vec::new(),
                        expanded: true,
                        view: TableView::default(),
                    });
                    self.next_id += 1;
                }
            }
        }
        cx.notify();
    }

    fn on_repl_enter(
        &mut self,
        secondary: bool,
        shift: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if shift {
            return;
        }
        if self.busy {
            return;
        }
        let source = self.repl.read(cx).value().to_string();
        let trimmed = source.trim().to_string();
        if trimmed.is_empty() {
            return;
        }
        let complete = self.pipeline_is_complete(&trimmed);
        if !complete && !secondary {
            self.repl.update(cx, |editor, cx| {
                editor.insert("\n", window, cx);
            });
            self.engine.parse(format!("{source}\n"));
            return;
        }
        self.submit(trimmed, window, cx);
    }

    fn pipeline_is_complete(&self, trimmed: &str) -> bool {
        if let Some(report) = &self.parse {
            if report.source.trim() == trimmed {
                return report.complete;
            }
        }
        crate::engine::source_looks_complete(trimmed)
    }

    fn submit(&mut self, source: String, window: &mut Window, cx: &mut Context<Self>) {
        for result in &mut self.results {
            result.expanded = false;
        }
        let id = ResultId(self.next_id);
        self.next_id += 1;
        self.results
            .push(InvocationResult::running(id, source.clone()));
        while self.results.len() > result_cap() {
            let removed = self.results.remove(0);
            self.text_lines.remove(&removed.id.0);
        }
        self.history.push(source.clone());
        self.reset_table_chrome();
        self.busy = true;
        self.engine.eval(id, source);
        self.repl.update(cx, |editor, cx| {
            editor.set_value("", window, cx);
        });
        cx.notify();
    }

    fn stop(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.engine.interrupt();
        cx.notify();
    }

    fn toggle_help(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.help_open = !self.help_open;
        if self.help_open {
            self.engine.scope_commands();
        }
        cx.notify();
    }

    fn toggle_history(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.history_open = !self.history_open;
        cx.notify();
    }

    fn toggle_variables(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.variables_open = !self.variables_open;
        if self.variables_open {
            self.engine.scope_variables();
        }
        cx.notify();
    }

    fn insert_history(&mut self, source: String, window: &mut Window, cx: &mut Context<Self>) {
        self.repl.update(cx, |editor, cx| {
            editor.set_value(&source, window, cx);
        });
        self.engine.parse(source);
        cx.notify();
    }

    fn expand_result(&mut self, id: ResultId, window: &mut Window, cx: &mut Context<Self>) {
        for result in &mut self.results {
            result.expanded = result.id == id;
        }
        if let Some(result) = self.results.iter().find(|r| r.id == id) {
            let search = result.view.search.clone();
            self.table_filter.update(cx, |input, cx| {
                input.set_value(search, window, cx);
            });
        }
        self.reset_table_chrome();
        self.refresh_result_table(cx);
        cx.notify();
    }

    fn open_settings(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.settings_open = true;
        cx.notify();
    }

    fn close_settings(&mut self, cx: &mut Context<Self>) {
        self.settings_open = false;
        cx.notify();
    }

    fn toggle_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mode = if cx.theme().mode.is_dark() {
            ThemeMode::Light
        } else {
            ThemeMode::Dark
        };
        Theme::change(mode, Some(window), cx);
        cx.notify();
    }

    fn copy_expanded(&mut self, format: CopyFormat, cx: &mut Context<Self>) {
        let Some(result) = self.results.iter().rev().find(|r| r.expanded) else {
            return;
        };
        let text = copy_result(&result.body, &result.view, format);
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.copy_status = format!("copied {}", format.label());
        cx.notify();
    }

    fn render_titlebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        TitleBar::new().child(
            h_flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .font_semibold()
                        .text_color(cx.theme().foreground)
                        .child("Sisko"),
                )
                .child(
                    Button::new("stop")
                        .ghost()
                        .xsmall()
                        .label("Stop")
                        .on_click(cx.listener(|this, _, window, cx| this.stop(window, cx))),
                )
                .child(
                    Button::new("toggle-help")
                        .ghost()
                        .xsmall()
                        .label("Help")
                        .on_click(cx.listener(|this, _, window, cx| this.toggle_help(window, cx))),
                )
                .child(
                    Button::new("toggle-history")
                        .ghost()
                        .xsmall()
                        .label("History")
                        .on_click(
                            cx.listener(|this, _, window, cx| this.toggle_history(window, cx)),
                        ),
                )
                .child(
                    Button::new("toggle-variables")
                        .ghost()
                        .xsmall()
                        .label("Variables")
                        .on_click(
                            cx.listener(|this, _, window, cx| this.toggle_variables(window, cx)),
                        ),
                )
                .child(
                    Button::new("toggle-theme")
                        .ghost()
                        .xsmall()
                        .label(if cx.theme().mode.is_dark() {
                            "Light"
                        } else {
                            "Dark"
                        })
                        .on_click(cx.listener(|this, _, window, cx| this.toggle_theme(window, cx))),
                )
                .child(
                    Button::new("open-settings")
                        .ghost()
                        .xsmall()
                        .label("Settings")
                        .on_click(
                            cx.listener(|this, _, window, cx| this.open_settings(window, cx)),
                        ),
                ),
        )
    }

    fn render_results(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let items: Vec<_> = self
            .results
            .iter()
            .map(|result| self.render_result_card(result, cx))
            .collect();

        v_flex()
            .id("result-stack")
            .flex_1()
            .min_h_0()
            .min_w_0()
            .h_full()
            .px_2()
            .pt_2()
            .gap_2()
            .children(if items.is_empty() {
                vec![
                    div()
                        .flex_1()
                        .items_center()
                        .justify_center()
                        .text_color(cx.theme().muted_foreground)
                        .child("Submit a pipeline to create a Result.")
                        .into_any_element(),
                ]
            } else {
                items
            })
    }

    fn render_result_card(
        &self,
        result: &InvocationResult,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let id = result.id;
        let header = h_flex()
            .id(SharedString::from(format!("result-header-{}", id.0)))
            .w_full()
            .items_center()
            .justify_between()
            .px_2()
            .py_1()
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, window, cx| this.expand_result(id, window, cx)))
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .min_w_0()
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(result.status_label()),
                    )
                    .child(
                        div()
                            .font_family(cx.theme().mono_font_family.clone())
                            .text_sm()
                            .child(result.source.clone()),
                    ),
            )
            .child(self.render_result_actions(result, cx));

        let body = if result.expanded {
            Some(self.render_body(result, cx).into_any_element())
        } else {
            None
        };

        v_flex()
            .w_full()
            .min_h_0()
            .when(result.expanded, |this| this.flex_1())
            .when(!result.expanded, |this| this.flex_shrink_0())
            .border_1()
            .border_color(cx.theme().border)
            .rounded(cx.theme().radius)
            .bg(cx.theme().background)
            .child(header)
            .children(body)
            .into_any_element()
    }

    fn render_result_actions(
        &self,
        result: &InvocationResult,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let duration = result.duration.map(format_duration).unwrap_or_default();
        h_flex()
            .gap_1()
            .items_center()
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(duration),
            )
            .when(result.expanded, |this| {
                this.when(matches!(result.body, ResultBody::Table(_)), |this| {
                    this.child(self.render_auto_size_button(result.id.0, cx))
                })
                .child(self.render_copy_menu(result.id, cx))
            })
    }

    fn render_auto_size_button(&self, id: u64, cx: &mut Context<Self>) -> impl IntoElement {
        Button::new(SharedString::from(format!("auto-size-{id}")))
            .ghost()
            .xsmall()
            .label("Auto-size")
            .on_click(cx.listener(|this, _, _, cx| this.auto_resize_visible_columns(cx)))
    }

    fn render_copy_menu(&self, id: ResultId, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity();
        Button::new(SharedString::from(format!("copy-as-{0}", id.0)))
            .ghost()
            .xsmall()
            .label("Copy as")
            .dropdown_menu(move |menu, _, _| {
                let mut menu = menu;
                for format in [
                    CopyFormat::Text,
                    CopyFormat::Tsv,
                    CopyFormat::Csv,
                    CopyFormat::Json,
                    CopyFormat::Nuon,
                    CopyFormat::Markdown,
                ] {
                    let view = view.clone();
                    menu = menu.item(
                        PopupMenuItem::new(format!("Copy as {}", format.label())).on_click(
                            move |_, _, cx| {
                                view.update(cx, |this, cx| this.copy_expanded(format, cx));
                            },
                        ),
                    );
                }
                menu
            })
    }

    fn render_body(&self, result: &InvocationResult, cx: &mut Context<Self>) -> impl IntoElement {
        let is_table = matches!(result.body, ResultBody::Table(_));
        let is_long_text = matches!(
            &result.body,
            ResultBody::Text(_) | ResultBody::Diagnostic(_)
        );
        let fill = is_table || is_long_text;
        div()
            .w_full()
            .min_h_0()
            .when(fill, |this| this.flex_1())
            .when(!fill, |this| this.max_h(px(240.)))
            .overflow_hidden()
            .border_t_1()
            .border_color(cx.theme().border)
            .p_2()
            .child(match &result.body {
                ResultBody::Empty => div()
                    .text_color(cx.theme().muted_foreground)
                    .child("empty")
                    .into_any_element(),
                ResultBody::Scalar(text) if text.contains('\n') => self
                    .render_scrollable_text(result.id, text, None, cx)
                    .into_any_element(),
                ResultBody::Scalar(text) => div()
                    .font_family(cx.theme().mono_font_family.clone())
                    .child(text.clone())
                    .into_any_element(),
                ResultBody::Text(text) => self
                    .render_scrollable_text(result.id, text, None, cx)
                    .into_any_element(),
                ResultBody::Diagnostic(text) => self
                    .render_scrollable_text(result.id, text, Some(cx.theme().danger), cx)
                    .into_any_element(),
                ResultBody::Binary { summary, .. } => {
                    div().child(summary.clone()).into_any_element()
                }
                ResultBody::Table(store)
                    if (store.is_pairs() || store.has_nested()) && !store.prefers_virtual() =>
                {
                    self.render_expandable_table(store, cx).into_any_element()
                }
                ResultBody::Table(store) => self.render_virtual_table(store, cx).into_any_element(),
            })
    }

    fn render_scrollable_text(
        &self,
        id: ResultId,
        text: &str,
        color: Option<gpui::Hsla>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let font = self.settings.repl_font_size;
        let dark = cx.theme().mode.is_dark();
        let fallback = color.unwrap_or(cx.theme().foreground);
        let family = cx.theme().mono_font_family.clone();
        let lines = self
            .text_lines
            .get(&id.0)
            .cloned()
            .unwrap_or_else(|| Arc::new(text.lines().map(SharedString::from).collect()));
        let styled = looks_like_ansi(text);
        let count = lines.len().max(1);
        let list_id = SharedString::from(format!("result-text-{}", id.0));
        uniform_list(list_id, count, move |range, _, _| {
            range
                .map(|ix| {
                    let line = lines.get(ix).cloned().unwrap_or_default();
                    if styled {
                        return h_flex()
                            .w_full()
                            .h(px(font + 6.))
                            .flex_shrink_0()
                            .px_1()
                            .font_family(family.clone())
                            .text_size(px(font))
                            .children(parse_ansi_lines(&line).into_iter().flatten().map(|span| {
                                let fg = span
                                    .fg
                                    .map(|rgb| rgb.contrast_on(dark).hsla())
                                    .unwrap_or(fallback);
                                div()
                                    .when_some(span.bg, |this, bg| {
                                        this.bg(bg.contrast_on(dark).hsla())
                                    })
                                    .text_color(fg)
                                    .font_weight(if span.bold {
                                        gpui::FontWeight::SEMIBOLD
                                    } else {
                                        gpui::FontWeight::NORMAL
                                    })
                                    .child(span.text)
                            }))
                            .into_any_element();
                    }
                    div()
                        .w_full()
                        .h(px(font + 6.))
                        .flex_shrink_0()
                        .px_1()
                        .font_family(family.clone())
                        .text_size(px(font))
                        .text_color(fallback)
                        .child(if line.is_empty() {
                            SharedString::from(" ")
                        } else {
                            line
                        })
                        .into_any_element()
                })
                .collect()
        })
        .w_full()
        .h_full()
    }

    fn render_expandable_table(
        &self,
        store: &ResultStore,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let kind = if store.is_pairs() { "fields" } else { "rows" };
        v_flex()
            .w_full()
            .h_full()
            .gap_1()
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(div().w(px(220.)).when(!store.is_pairs(), |this| {
                        this.child(Input::new(&self.table_filter).small())
                    }))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!(
                                "{} {kind} · click ▸ to expand under the row",
                                store.row_count()
                            )),
                    )
                    .child(self.render_auto_size_button(0, cx)),
            )
            .child(
                div()
                    .w_full()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_inspect_table("root", store, true, cx)),
            )
    }

    fn render_virtual_table(
        &self,
        store: &ResultStore,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let expansions = self.root_expansions(store);
        v_flex()
            .w_full()
            .h_full()
            .min_h_0()
            .gap_2()
            .child(
                h_flex()
                    .w_full()
                    .h(px(28.))
                    .flex_shrink_0()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .w(px(220.))
                            .child(Input::new(&self.table_filter).small()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!(
                                "{} of {} rows · filter to find a command, ▸ expands below",
                                self.results
                                    .iter()
                                    .rev()
                                    .find(|r| r.expanded)
                                    .map(|r| store.visible_indices(&r.view).len())
                                    .unwrap_or(store.row_count()),
                                store.row_count()
                            )),
                    )
                    .child(self.render_auto_size_button(1, cx)),
            )
            .child(
                div()
                    .relative()
                    .w_full()
                    .flex_1()
                    .min_h(px(120.))
                    .min_w_0()
                    .child(measure_strip(cx.entity(), Some("virtual".into()), None))
                    .child(
                        DataTable::new(&self.result_table)
                            .stripe(true)
                            .bordered(true),
                    ),
            )
            .children(expansions.into_iter().map(|(key, title, nested)| {
                let collapse = key.clone();
                v_flex()
                    .w_full()
                    .flex_1()
                    .min_h(px(180.))
                    .min_w_0()
                    .gap_1()
                    .border_1()
                    .border_color(cx.theme().border)
                    .rounded(px(4.))
                    .p_1()
                    .bg(cx.theme().secondary.opacity(0.28))
                    .child(
                        h_flex()
                            .w_full()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(format!(
                                        "▾ {title} · {}",
                                        if nested.is_text_block() {
                                            let n = nested
                                                .rows
                                                .first()
                                                .and_then(|row| row.first())
                                                .map(Cell::line_count)
                                                .unwrap_or(0);
                                            if n == 1 {
                                                "1 line".into()
                                            } else {
                                                format!("{n} lines")
                                            }
                                        } else {
                                            format!(
                                                "{} rows × {} cols",
                                                nested.row_count(),
                                                nested.columns.len()
                                            )
                                        }
                                    )),
                            )
                            .child(
                                Button::new(SharedString::from(format!("collapse-{key}")))
                                    .ghost()
                                    .xsmall()
                                    .label("Collapse")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.toggle_inline_expand(collapse.clone(), cx);
                                    })),
                            ),
                    )
                    .child(
                        div()
                            .w_full()
                            .flex_1()
                            .min_h_0()
                            .child(self.render_inspect_table(&key, &nested, true, cx)),
                    )
                    .into_any_element()
            }))
    }

    fn root_expansions(&self, root: &ResultStore) -> Vec<(String, String, ResultStore)> {
        let mut keys: Vec<String> = self
            .inline_expand
            .iter()
            .filter(|key| parse_root_row(key).is_some())
            .cloned()
            .collect();
        keys.sort();
        keys.into_iter()
            .filter_map(|key| {
                let store = nested_store(root, &key)?.clone();
                let col = key.split('/').nth(2)?.parse::<usize>().ok()?;
                let title = root
                    .columns
                    .get(col)
                    .cloned()
                    .unwrap_or_else(|| "nested".into());
                Some((key, title, store))
            })
            .collect()
    }

    fn render_inspect_table(
        &self,
        path: &str,
        store: &ResultStore,
        fill: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        if store.is_text_block() {
            let text = store
                .rows
                .first()
                .and_then(|row| row.first())
                .map(|cell| cell.text.as_str())
                .unwrap_or("");
            return self.render_text_block(path, text, fill, cx);
        }
        let font = self.settings.table_font_size;
        let dark = cx.theme().mode.is_dark();
        let widths = self.inspect_column_widths(path, store);
        let table_w = INDEX_COL_PX + widths.iter().sum::<f32>();
        let path_owned = path.to_string();

        let header = h_flex()
            .w(px(table_w))
            .min_w(px(table_w))
            .h(px(28.))
            .flex_shrink_0()
            .bg(cx.theme().secondary)
            .border_b_1()
            .border_color(cx.theme().border)
            .child(inspect_index_cell("#", font, true, cx))
            .children(
                store
                    .columns
                    .iter()
                    .enumerate()
                    .map(|(ix, name)| self.render_inspect_header_cell(name, widths[ix])),
            );

        let indices: Vec<usize> = if path == "root" {
            self.results
                .iter()
                .rev()
                .find(|result| result.expanded)
                .map(|result| store.visible_indices(&result.view))
                .unwrap_or_else(|| (0..store.rows.len()).collect())
        } else {
            (0..store.rows.len()).collect()
        };

        let mut rows: Vec<gpui::AnyElement> = Vec::new();
        for (vis_ix, &ix) in indices.iter().enumerate() {
            let Some(row) = store.rows.get(ix) else {
                continue;
            };
            rows.push(
                h_flex()
                    .w(px(table_w))
                    .min_w(px(table_w))
                    .flex_shrink_0()
                    .items_start()
                    .border_b_1()
                    .border_color(cx.theme().border.opacity(0.45))
                    .bg(if vis_ix % 2 == 0 {
                        cx.theme().background
                    } else {
                        cx.theme().secondary.opacity(0.22)
                    })
                    .child(inspect_index_cell(&(ix + 1).to_string(), font, false, cx))
                    .children(store.columns.iter().enumerate().map(|(col, name)| {
                        let cell = row.get(col).cloned().unwrap_or_default();
                        let key = table_path_key(path, ix, col);
                        self.render_inspect_cell(&key, name, &cell, widths[col], font, dark, cx)
                            .into_any_element()
                    }))
                    .into_any_element(),
            );

            for (col, name) in store.columns.iter().enumerate() {
                let key = table_path_key(path, ix, col);
                if !self.inline_expand.contains(&key) {
                    continue;
                }
                let Some(nested) = row.get(col).and_then(|cell| cell.nested.as_deref()) else {
                    continue;
                };
                rows.push(self.render_inline_nested(&key, name, nested, table_w, cx));
            }
        }

        let rails: Vec<_> = widths
            .iter()
            .enumerate()
            .map(|(col, &width)| {
                let left_offset = INDEX_COL_PX + widths.iter().take(col).sum::<f32>();
                self.render_col_resize_rail(path, col, left_offset, width, cx)
            })
            .collect();

        let stacked = div()
            .relative()
            .w(px(table_w))
            .min_w(px(table_w))
            .flex_shrink_0()
            .child(
                v_flex()
                    .w(px(table_w))
                    .min_w(px(table_w))
                    .flex_shrink_0()
                    .child(header)
                    .children(rows),
            )
            .child(measure_strip(cx.entity(), None, Some(path_owned.clone())))
            .children(rails);

        if fill {
            div()
                .id(SharedString::from(format!("inspect-table-{path_owned}")))
                .relative()
                .w_full()
                .h_full()
                .min_h_0()
                .child(measure_strip(cx.entity(), Some(path_owned.clone()), None))
                .overflow_y_scrollbar()
                .child(stacked)
                .into_any_element()
        } else {
            div()
                .id(SharedString::from(format!("inspect-table-{path_owned}")))
                .relative()
                .w_full()
                .flex_shrink_0()
                .child(measure_strip(cx.entity(), Some(path_owned.clone()), None))
                .child(stacked)
                .into_any_element()
        }
    }

    fn render_inspect_header_cell(&self, name: &str, width: f32) -> gpui::AnyElement {
        div()
            .w(px(width))
            .min_w(px(width))
            .h_full()
            .flex_shrink_0()
            .px_2()
            .flex()
            .items_center()
            .text_xs()
            .font_semibold()
            .child(name.to_string())
            .into_any_element()
    }

    fn render_col_resize_rail(
        &self,
        path: &str,
        col: usize,
        left_offset: f32,
        width: f32,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let path = path.to_string();
        let drag_path = path.clone();
        div()
            .id(SharedString::from(format!("resize-rail-{path}-{col}")))
            .absolute()
            .top_0()
            .bottom_0()
            .left(px(left_offset + width - 4.0))
            .w(px(8.))
            .occlude()
            .cursor_col_resize()
            .flex()
            .justify_center()
            .child(div().w(px(1.)).h_full().bg(cx.theme().border.opacity(0.65)))
            .on_drag(
                ResizeInspectCol {
                    path: path.clone(),
                    col,
                    left_offset,
                },
                |drag, _, _, cx| {
                    cx.stop_propagation();
                    cx.new(|_| drag.clone())
                },
            )
            .on_drag_move(
                cx.listener(move |this, e: &DragMoveEvent<ResizeInspectCol>, _, cx| {
                    let spec = e.drag(cx).clone();
                    if spec.path != drag_path || spec.col != col {
                        return;
                    }
                    this.drag_resize_column(&spec, f32::from(e.event.position.x), cx);
                }),
            )
            .into_any_element()
    }

    fn render_text_block(
        &self,
        path: &str,
        text: &str,
        fill: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let font = self.settings.table_font_size;
        let dark = cx.theme().mode.is_dark();
        let fallback = cx.theme().foreground;
        let rendered = render_ansi_line_elements(text, font, fallback, dark, cx);
        let body = v_flex().w_full().flex_shrink_0().children(rendered);
        if fill {
            div()
                .id(SharedString::from(format!("text-block-{path}")))
                .w_full()
                .h_full()
                .min_h_0()
                .overflow_y_scrollbar()
                .child(body)
                .into_any_element()
        } else {
            div()
                .id(SharedString::from(format!("text-block-{path}")))
                .w_full()
                .flex_shrink_0()
                .child(body)
                .into_any_element()
        }
    }

    fn render_inline_nested(
        &self,
        path: &str,
        column: &str,
        store: &ResultStore,
        table_w: f32,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        div()
            .w(px(table_w))
            .min_w(px(table_w))
            .flex_shrink_0()
            .pl(px(48.))
            .pr_2()
            .py_1()
            .bg(cx.theme().secondary.opacity(0.35))
            .border_b_1()
            .border_color(cx.theme().border.opacity(0.45))
            .child(
                v_flex()
                    .w_full()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!(
                                "{column} · {}",
                                if store.is_text_block() {
                                    let n = store
                                        .rows
                                        .first()
                                        .and_then(|row| row.first())
                                        .map(Cell::line_count)
                                        .unwrap_or(0);
                                    if n == 1 {
                                        "1 line".into()
                                    } else {
                                        format!("{n} lines")
                                    }
                                } else {
                                    format!(
                                        "{} rows × {} cols",
                                        store.row_count(),
                                        store.columns.len()
                                    )
                                }
                            )),
                    )
                    .child(
                        div()
                            .w_full()
                            .border_1()
                            .border_color(cx.theme().border)
                            .rounded(px(3.))
                            .bg(cx.theme().background)
                            .child(if store.prefers_virtual() {
                                div()
                                    .w_full()
                                    .h(px(420.))
                                    .min_h(px(240.))
                                    .min_w_0()
                                    .child(
                                        DataTable::new(&self.nested_table)
                                            .stripe(true)
                                            .bordered(true),
                                    )
                                    .into_any_element()
                            } else {
                                self.render_inspect_table(path, store, false, cx)
                            }),
                    ),
            )
            .into_any_element()
    }

    fn render_inspect_cell(
        &self,
        expand_key: &str,
        column: &str,
        cell: &Cell,
        width: f32,
        font: f32,
        dark: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .w(px(width))
            .min_w(px(width))
            .flex_shrink_0()
            .px_2()
            .py_1()
            .child(self.render_expanded_cell(
                expand_key,
                column,
                cell,
                font,
                dark,
                Some(expand_key.to_string()),
                cx,
            ))
    }

    fn render_expanded_cell(
        &self,
        id: &str,
        column: &str,
        cell: &Cell,
        font: f32,
        dark: bool,
        expand_key: Option<String>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        if cell.nested.is_some() {
            let key = expand_key.unwrap_or_else(|| id.to_string());
            let expanded = self.inline_expand.contains(&key);
            return div()
                .id(SharedString::from(format!("inspect-cell-{id}")))
                .px_1p5()
                .py_0p5()
                .rounded(px(3.))
                .border_1()
                .border_color(cx.theme().border)
                .bg(cx.theme().secondary)
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.toggle_inline_expand(key.clone(), cx);
                }))
                .child(
                    div()
                        .text_size(px(font))
                        .font_family(cx.theme().mono_font_family.clone())
                        .text_color(cx.theme().info)
                        .child(format!(
                            "{} {}",
                            if expanded { "▾" } else { "▸" },
                            empty_dash(&cell.expand_label())
                        )),
                )
                .into_any_element();
        }

        if looks_like_ansi(&cell.text) && !cell.text.trim().is_empty() {
            return v_flex()
                .w_full()
                .children(render_ansi_line_elements(
                    &cell.text,
                    font,
                    cx.theme().foreground,
                    dark,
                    cx,
                ))
                .into_any_element();
        }
        let highlight = is_code_column(column) || looks_like_nu_value(&cell.text);
        if highlight && !cell.text.trim().is_empty() {
            render_highlighted_nu(&cell.text, font, dark, cx).into_any_element()
        } else {
            div()
                .w_full()
                .font_family(cx.theme().mono_font_family.clone())
                .text_size(px(font))
                .text_color(cx.theme().foreground)
                .child(empty_dash(&cell.text))
                .into_any_element()
        }
    }

    fn render_help(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let filter = self.help_filter.read(cx).value().to_lowercase();
        let font = self.settings.table_font_size;
        let items: Vec<_> = self
            .help_commands
            .rows
            .iter()
            .filter(|row| {
                if filter.is_empty() {
                    return true;
                }
                row.iter()
                    .any(|cell| cell.text.to_lowercase().contains(&filter))
            })
            .take(400)
            .map(|row| {
                let name = row.first().map(|c| c.text.clone()).unwrap_or_default();
                let category = row.get(1).map(|c| c.text.clone()).unwrap_or_default();
                let kind = row.get(2).map(|c| c.text.clone()).unwrap_or_default();
                let description = row.get(3).map(|c| c.text.clone()).unwrap_or_default();
                let insert = name.clone();
                div()
                    .id(SharedString::from(format!("help-{}", element_key(&name))))
                    .px_2()
                    .py_1()
                    .cursor_pointer()
                    .hover(|s| s.bg(cx.theme().accent.opacity(0.15)))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.insert_history(insert.clone(), window, cx);
                    }))
                    .child(
                        v_flex()
                            .gap_0()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(
                                        div()
                                            .font_family(cx.theme().mono_font_family.clone())
                                            .text_size(px(font))
                                            .font_semibold()
                                            .child(name),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(format!("{kind} · {category}")),
                                    ),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(description),
                            ),
                    )
                    .into_any_element()
            })
            .collect();

        v_flex()
            .w(px(320.))
            .h_full()
            .flex_shrink_0()
            .border_l_1()
            .border_color(cx.theme().border)
            .child(div().px_2().py_1().font_semibold().child("Help"))
            .child(
                div()
                    .px_2()
                    .pb_1()
                    .child(Input::new(&self.help_filter).small()),
            )
            .child(
                div()
                    .px_2()
                    .pb_1()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(if self.help_commands.row_count() == 0 {
                        "Loading `scope commands`…".into()
                    } else {
                        format!("{} commands · click to insert", items.len())
                    }),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .id("help-list")
                    .overflow_y_scrollbar()
                    .children(items),
            )
    }

    fn render_history(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let items: Vec<_> = self
            .history
            .iter()
            .rev()
            .take(50)
            .map(|source| {
                let source = source.clone();
                div()
                    .id(SharedString::from(format!("hist-{source}")))
                    .px_2()
                    .py_1()
                    .cursor_pointer()
                    .hover(|s| s.bg(cx.theme().accent.opacity(0.15)))
                    .child(source.clone())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.insert_history(source.clone(), window, cx)
                    }))
                    .into_any_element()
            })
            .collect();

        v_flex()
            .w(px(240.))
            .h_full()
            .flex_shrink_0()
            .border_r_1()
            .border_color(cx.theme().border)
            .child(div().px_2().py_1().font_semibold().child("History"))
            .child(
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .id("history-list")
                    .overflow_y_scrollbar()
                    .children(items),
            )
    }

    fn render_variables(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let font = self.settings.table_font_size;
        let rows: Vec<_> = self
            .variables
            .rows
            .iter()
            .flat_map(|row| {
                let name = row.first().map(|c| c.text.clone()).unwrap_or_default();
                let ty = row.get(1).map(|c| c.text.clone()).unwrap_or_default();
                let value = row.get(2).cloned().unwrap_or_default();
                let mut items = vec![
                    self.render_variable_row(&name, &name, &ty, &value, true, 0, font, cx)
                        .into_any_element(),
                ];
                if self.var_expand.contains(&name) {
                    if let Some(nested) = value.nested.as_deref() {
                        items.extend(self.render_variable_nested(&name, nested, 1, font, cx));
                    }
                }
                items
            })
            .collect();

        v_flex()
            .w(px(360.))
            .h_full()
            .flex_shrink_0()
            .border_l_1()
            .border_color(cx.theme().border)
            .child(div().px_2().py_1().font_semibold().child("Variables"))
            .child(
                div()
                    .px_2()
                    .pb_1()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(
                        if self.variables.columns.first().map(String::as_str) == Some("error") {
                            self.variables
                                .rows
                                .first()
                                .and_then(|row| row.first())
                                .map(|cell| format!("error: {}", cell.text))
                                .unwrap_or_else(|| "Variables query failed".into())
                        } else if self.variables.row_count() == 0 {
                            "Loading `scope variables`…".into()
                        } else {
                            format!(
                                "{} bindings · click name to insert, ▸ to expand",
                                self.variables.row_count()
                            )
                        },
                    ),
            )
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(div().w(px(140.)).text_xs().font_semibold().child("Name"))
                    .child(div().w(px(72.)).text_xs().font_semibold().child("Type"))
                    .child(div().flex_1().text_xs().font_semibold().child("Value")),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .id("variables-list")
                    .overflow_y_scrollbar()
                    .children(rows),
            )
    }

    fn render_variable_row(
        &self,
        display: &str,
        expand_key: &str,
        ty: &str,
        value: &Cell,
        insertable: bool,
        indent: usize,
        font: f32,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let insert = display.to_string();
        let key = expand_key.to_string();
        let expanded = self.var_expand.contains(&key);
        h_flex()
            .id(SharedString::from(format!(
                "var-row-{}",
                element_key(expand_key)
            )))
            .w_full()
            .px_2()
            .py_1()
            .pl(px(8.0 + indent as f32 * 14.0))
            .hover(|s| s.bg(cx.theme().accent.opacity(0.12)))
            .child(
                div()
                    .id(SharedString::from(format!(
                        "var-name-{}",
                        element_key(expand_key)
                    )))
                    .w(px((140.0 - indent as f32 * 8.0).max(64.0)))
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_size(px(font))
                    .font_semibold()
                    .when(insertable, |this| {
                        this.cursor_pointer()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.insert_history(insert.clone(), window, cx);
                            }))
                    })
                    .child(display.to_string()),
            )
            .child(
                div()
                    .w(px(72.))
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(ty.to_string()),
            )
            .child(if let Some(nested) = value.nested.clone() {
                let key = key.clone();
                div()
                    .id(SharedString::from(format!(
                        "var-expand-{}",
                        element_key(&key)
                    )))
                    .flex_1()
                    .px_1p5()
                    .rounded(px(3.))
                    .border_1()
                    .border_color(cx.theme().border)
                    .bg(cx.theme().secondary)
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if this.var_expand.contains(&key) {
                            this.var_expand.remove(&key);
                        } else {
                            this.var_expand.insert(key.clone());
                        }
                        let _ = nested;
                        cx.notify();
                    }))
                    .child(
                        div()
                            .text_size(px(font))
                            .font_family(cx.theme().mono_font_family.clone())
                            .text_color(cx.theme().info)
                            .child(format!(
                                "{} {}",
                                if expanded { "▾" } else { "▸" },
                                empty_dash(&value.text)
                            )),
                    )
                    .into_any_element()
            } else {
                div()
                    .flex_1()
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_size(px(font))
                    .text_color(cx.theme().foreground)
                    .child(empty_dash(&value.text))
                    .into_any_element()
            })
    }

    fn render_variable_nested(
        &self,
        parent: &str,
        store: &ResultStore,
        indent: usize,
        font: f32,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let is_pairs = store.columns == ["field".to_string(), "value".to_string()];
        store
            .rows
            .iter()
            .take(80)
            .flat_map(|row| {
                let (display, value) = if is_pairs {
                    let field = row.first().map(|c| c.text.clone()).unwrap_or_default();
                    let val = row.get(1).cloned().unwrap_or_default();
                    (field, val)
                } else {
                    let label = row
                        .first()
                        .map(|c| c.text.clone())
                        .unwrap_or_else(|| "row".into());
                    (label, row.first().cloned().unwrap_or_default())
                };
                let key = format!("{parent}/{display}");
                let mut items = vec![
                    self.render_variable_row(&display, &key, "", &value, false, indent, font, cx)
                        .into_any_element(),
                ];
                if self.var_expand.contains(&key) {
                    if let Some(nested) = value.nested.as_deref() {
                        items.extend(self.render_variable_nested(
                            &key,
                            nested,
                            indent + 1,
                            font,
                            cx,
                        ));
                    }
                }
                items
            })
            .collect()
    }

    fn render_repl(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let parse_label = self
            .parse
            .as_ref()
            .and_then(|p| p.error.clone())
            .unwrap_or_else(|| {
                if self.parse.as_ref().is_some_and(|p| p.complete) {
                    "ready".into()
                } else {
                    "incomplete".into()
                }
            });

        v_flex()
            .w_full()
            .border_t_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .justify_between()
                    .child(div().text_xs().child("REPL"))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(parse_label),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .h(px(96.))
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_size(px(self.settings.repl_font_size))
                    .child(
                        Editor::new(&self.repl)
                            .h(px(96.))
                            .font_family(cx.theme().mono_font_family.clone())
                            .text_size(px(self.settings.repl_font_size)),
                    ),
            )
    }

    fn render_statusbar(&self, _cx: &mut Context<Self>) -> impl IntoElement {
        let engine = if self.busy { "busy" } else { "idle" };
        let parse = self
            .parse
            .as_ref()
            .map(|p| {
                if p.error.is_some() {
                    "parse error"
                } else if p.complete {
                    "parse ok"
                } else {
                    "incomplete"
                }
            })
            .unwrap_or("parse …");
        let rows = self
            .results
            .iter()
            .rev()
            .find(|r| r.expanded)
            .and_then(|r| match &r.body {
                ResultBody::Table(store) => {
                    let visible = store.visible_indices(&r.view).len();
                    Some(format!("{visible} / {} rows", store.row_count()))
                }
                _ => None,
            })
            .unwrap_or_default();
        let duration = self.last_duration.map(format_duration).unwrap_or_default();
        let exit = self
            .last_exit
            .map(|code| format!("exit {code}"))
            .unwrap_or_default();

        StatusBar::new()
            .left(self.cwd.clone())
            .left(if self.snapshot.config_path.is_empty() {
                engine.to_string()
            } else {
                format!("{engine} · {}", self.snapshot.config_path)
            })
            .child(rows)
            .right(parse)
            .right(exit)
            .right(duration)
            .right(if self.copy_status.is_empty() {
                String::new()
            } else {
                self.copy_status.clone()
            })
            .right(format!("nu {}", self.nu_version))
    }

    fn render_settings_modal(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        div()
            .id("settings-overlay")
            .absolute()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::hsla(0., 0., 0., 0.45))
            .occlude()
            .child(
                v_flex()
                    .id("settings-card")
                    .w(px(440.))
                    .p_4()
                    .gap_4()
                    .rounded(cx.theme().radius_lg)
                    .border_1()
                    .border_color(cx.theme().border)
                    .bg(cx.theme().background)
                    .shadow_lg()
                    .child(
                        h_flex()
                            .w_full()
                            .items_center()
                            .justify_between()
                            .child(div().text_lg().font_semibold().child("Settings"))
                            .child(
                                Button::new("settings-close")
                                    .ghost()
                                    .xsmall()
                                    .label("Close")
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.close_settings(cx)),
                                    ),
                            ),
                    )
                    .child(
                        v_flex()
                            .gap_1()
                            .child(div().text_sm().font_semibold().child(format!(
                                "REPL font size · {}px",
                                self.settings.repl_font_size
                            )))
                            .child(
                                div().text_xs().text_color(muted).child(
                                    "Size of the pipeline editor at the bottom of the window.",
                                ),
                            )
                            .child(Slider::new(&self.repl_font_slider).horizontal()),
                    )
                    .child(
                        v_flex()
                            .gap_1()
                            .child(div().text_sm().font_semibold().child(format!(
                                "Table font size · {}px",
                                self.settings.table_font_size
                            )))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(muted)
                                    .child("Size of result, inspect, Help, and Variables tables."),
                            )
                            .child(Slider::new(&self.table_font_slider).horizontal()),
                    )
                    .child(
                        h_flex().justify_end().child(
                            Button::new("settings-done")
                                .primary()
                                .label("Done")
                                .on_click(cx.listener(|this, _, _, cx| this.close_settings(cx))),
                        ),
                    ),
            )
    }
}

fn poll_engine(cx: &mut Context<Workspace>) {
    cx.spawn(async move |this, cx| {
        loop {
            cx.background_executor()
                .timer(Duration::from_millis(16))
                .await;
            if this
                .update(cx, |workspace, cx| workspace.drain_events(cx))
                .is_err()
            {
                break;
            }
        }
    })
    .detach();
}

fn is_code_column(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "example" | "examples" | "source" | "code" | "command" | "result"
    )
}

fn looks_like_nu_value(text: &str) -> bool {
    let trimmed = text.trim();
    trimmed.starts_with('[') || trimmed.starts_with('{') || trimmed.contains(" | ")
}

const INDEX_COL_PX: f32 = 48.0;
const SCROLLBAR_PX: f32 = 14.0;

fn measure_strip(
    view: Entity<Workspace>,
    viewport_key: Option<String>,
    origin_key: Option<String>,
) -> impl IntoElement {
    canvas(
        move |bounds, _, cx| {
            view.update(cx, |this, _| {
                if let Some(key) = viewport_key.as_ref() {
                    this.table_viewport_w
                        .insert(key.clone(), f32::from(bounds.size.width));
                }
                if let Some(key) = origin_key.as_ref() {
                    this.note_table_origin(key, f32::from(bounds.origin.x));
                }
            });
        },
        |_, _, _, _| {},
    )
    .absolute()
    .top_0()
    .left_0()
    .w_full()
    .h(px(1.))
}

#[derive(Clone)]
pub(crate) struct ResizeInspectCol {
    pub path: String,
    pub col: usize,
    pub left_offset: f32,
}

impl Render for ResizeInspectCol {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

fn parse_root_row(key: &str) -> Option<usize> {
    let mut parts = key.split('/');
    if parts.next()? != "root" {
        return None;
    }
    let row = parts.next()?.parse().ok()?;
    let _col = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    Some(row)
}

fn table_path_key(path: &str, row: usize, col: usize) -> String {
    if path.is_empty() {
        format!("{row}/{col}")
    } else {
        format!("{path}/{row}/{col}")
    }
}

fn nested_store<'a>(root: &'a ResultStore, path: &str) -> Option<&'a ResultStore> {
    let mut parts = path.split('/');
    let first = parts.next()?;
    if first != "root" && first != "sheet" {
        return None;
    }
    let mut current = root;
    loop {
        match (parts.next(), parts.next()) {
            (None, None) => return Some(current),
            (Some(row), Some(col)) => {
                let row: usize = row.parse().ok()?;
                let col: usize = col.parse().ok()?;
                current = current.rows.get(row)?.get(col)?.nested.as_deref()?;
            }
            _ => return None,
        }
    }
}

fn column_flex_weight(name: &str) -> f32 {
    match name {
        "value" | "result" | "example" | "examples" | "source" | "code" | "command" => 4.0,
        "field" | "name" | "type" | "kind" => 1.0,
        _ => 2.0,
    }
}

/// Grow columns to fill `available`, keeping each at least its preferred width.
fn fill_column_widths(names: &[String], preferred: &[f32], available: f32) -> Vec<f32> {
    let n = names.len().min(preferred.len());
    if n == 0 {
        return Vec::new();
    }
    let mut widths: Vec<f32> = preferred.iter().take(n).map(|&w| w.max(72.0)).collect();
    let sum: f32 = widths.iter().sum();
    if available <= sum + 0.5 {
        return widths;
    }
    let extra = available - sum;
    let mut weights: Vec<f32> = names
        .iter()
        .take(n)
        .map(|name| column_flex_weight(name))
        .collect();
    if weights.iter().sum::<f32>() <= 0.0 {
        weights.fill(1.0);
    }
    let weight_sum: f32 = weights.iter().sum();
    let mut assigned = 0.0;
    for i in 0..n {
        let add = if i + 1 == n {
            (extra - assigned).max(0.0)
        } else {
            (extra * weights[i] / weight_sum).floor()
        };
        widths[i] += add;
        assigned += add;
    }
    widths
}

fn merge_column_widths(stored: Option<&Vec<f32>>, defaults: &[f32]) -> Vec<f32> {
    defaults
        .iter()
        .enumerate()
        .map(|(ix, &def)| {
            stored
                .and_then(|widths| widths.get(ix).copied())
                .filter(|&width| width >= 72.0)
                .unwrap_or(def)
        })
        .collect()
}

fn inspect_column_width(name: &str, measured: f32) -> f32 {
    match name {
        name if is_code_column(name) || name == "value" || name == "result" => {
            measured.clamp(280.0, 720.0)
        }
        _ => measured.clamp(88.0, 360.0),
    }
}

fn element_key(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn empty_dash(text: &str) -> String {
    if text.trim().is_empty() {
        "—".into()
    } else {
        text.to_string()
    }
}

fn inspect_index_cell(
    text: &str,
    font: f32,
    header: bool,
    cx: &Context<Workspace>,
) -> impl IntoElement {
    div()
        .w(px(INDEX_COL_PX))
        .min_w(px(INDEX_COL_PX))
        .flex_shrink_0()
        .px_1()
        .py_1()
        .flex()
        .justify_end()
        .text_size(px(font))
        .text_color(if header {
            cx.theme().muted_foreground
        } else {
            cx.theme().info
        })
        .child(text.to_string())
}

fn render_ansi_line_elements(
    text: &str,
    font: f32,
    fallback: gpui::Hsla,
    dark: bool,
    cx: &Context<Workspace>,
) -> Vec<gpui::AnyElement> {
    parse_ansi_lines(text)
        .into_iter()
        .map(|line| {
            if line.is_empty() {
                return div()
                    .w_full()
                    .h(px(font + 4.))
                    .flex_shrink_0()
                    .into_any_element();
            }
            h_flex()
                .w_full()
                .flex_shrink_0()
                .flex_wrap()
                .font_family(cx.theme().mono_font_family.clone())
                .text_size(px(font))
                .children(line.into_iter().map(|span| {
                    let fg = span
                        .fg
                        .map(|rgb| rgb.contrast_on(dark).hsla())
                        .unwrap_or(fallback);
                    div()
                        .when_some(span.bg, |this, bg| this.bg(bg.contrast_on(dark).hsla()))
                        .text_color(fg)
                        .font_weight(if span.bold {
                            gpui::FontWeight::SEMIBOLD
                        } else {
                            gpui::FontWeight::NORMAL
                        })
                        .child(span.text)
                }))
                .into_any_element()
        })
        .collect()
}

fn render_highlighted_nu(
    source: &str,
    font: f32,
    dark: bool,
    cx: &Context<Workspace>,
) -> impl IntoElement {
    let lines = highlight_lines(source, dark);
    v_flex()
        .w_full()
        .font_family(cx.theme().mono_font_family.clone())
        .text_size(px(font))
        .children(lines.into_iter().map(|line| {
            if line.is_empty() {
                return div().h(px(font + 4.)).into_any_element();
            }
            h_flex()
                .w_full()
                .flex_wrap()
                .children(line.into_iter().map(|(text, style)| {
                    div()
                        .when_some(style.color, |this, color| this.text_color(color))
                        .when(style.font_weight.is_some(), |this| {
                            this.font_weight(gpui::FontWeight::SEMIBOLD)
                        })
                        .child(text)
                }))
                .into_any_element()
        }))
}

fn format_duration(duration: Duration) -> String {
    if duration.as_secs() >= 1 {
        format!("{:.2}s", duration.as_secs_f64())
    } else {
        format!("{}ms", duration.as_millis())
    }
}

impl Focusable for Workspace {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .relative()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .on_action(cx.listener(|this, _: &OpenSettings, window, cx| {
                this.open_settings(window, cx);
            }))
            .on_action(cx.listener(|this, _: &AutoResizeColumns, _, cx| {
                this.auto_resize_visible_columns(cx);
            }))
            .child(self.render_titlebar(cx))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .when(self.history_open, |this| {
                        this.child(self.render_history(cx))
                    })
                    .child(self.render_results(cx))
                    .when(self.help_open, |this| this.child(self.render_help(cx)))
                    .when(self.variables_open, |this| {
                        this.child(self.render_variables(cx))
                    }),
            )
            .child(self.render_repl(cx))
            .child(self.render_statusbar(cx))
            .when(self.settings_open, |this| {
                this.child(self.render_settings_modal(cx))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_root_row_only_direct_children() {
        assert_eq!(parse_root_row("root/4/2"), Some(4));
        assert_eq!(parse_root_row("root/4/2/0/1"), None);
        assert_eq!(parse_root_row("sheet/1/0"), None);
    }

    #[test]
    fn table_path_key_nests() {
        assert_eq!(table_path_key("root", 3, 1), "root/3/1");
        assert_eq!(table_path_key("root/3/1", 0, 2), "root/3/1/0/2");
    }

    #[test]
    fn merge_column_widths_keeps_manual_and_falls_back() {
        let defaults = vec![100.0, 280.0, 120.0];
        assert_eq!(
            merge_column_widths(Some(&vec![200.0, 0.0]), &defaults),
            vec![200.0, 280.0, 120.0]
        );
        assert_eq!(merge_column_widths(None, &defaults), defaults);
    }

    #[test]
    fn fill_column_widths_grows_to_available_and_prefers_value() {
        let names = vec!["field".into(), "value".into()];
        let filled = fill_column_widths(&names, &[120.0, 200.0], 800.0);
        assert!((filled.iter().sum::<f32>() - 800.0).abs() < 1.0);
        assert!(filled[0] >= 120.0);
        assert!(filled[1] >= 200.0);
        assert!(filled[1] > filled[0]);
    }

    #[test]
    fn fill_column_widths_does_not_shrink_below_preferred() {
        let names = vec!["field".into(), "value".into()];
        let filled = fill_column_widths(&names, &[400.0, 500.0], 200.0);
        assert_eq!(filled, vec![400.0, 500.0]);
    }

    #[test]
    fn nested_store_walks_row_col_pairs() {
        let inner = ResultStore {
            columns: vec!["a".into()],
            rows: vec![vec![Cell::text("1")]],
        };
        let mut cell = Cell::text("62 rows");
        cell.nested = Some(Box::new(inner.clone()));
        let root = ResultStore {
            columns: vec!["field".into(), "value".into()],
            rows: vec![
                vec![Cell::text("hooks"), Cell::text("x")],
                vec![Cell::text("table"), cell],
            ],
        };
        assert_eq!(nested_store(&root, "root").map(|s| s.row_count()), Some(2));
        assert_eq!(
            nested_store(&root, "root/1/1").map(|s| s.columns.clone()),
            Some(vec!["a".into()])
        );
        assert!(nested_store(&root, "root/0/0").is_none());
    }
}
