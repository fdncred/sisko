//! Virtualized Result table.

use gpui::{
    App, AppContext, Context, DragMoveEvent, FontWeight, InteractiveElement, IntoElement,
    ParentElement, SharedString, StatefulInteractiveElement, Styled, WeakEntity, Window, canvas,
    div, prelude::FluentBuilder, px,
};
use gpui_component::ActiveTheme;
use gpui_component::table::{Column, ColumnFixed, ColumnSort, TableDelegate, TableState};

use crate::ansi::{looks_like_ansi, parse_ansi_lines};
use crate::color::Rgb;
use crate::result::{Cell, ResultStore, TableView};
use crate::workspace::{ResizeInspectCol, Workspace};

pub struct ResultTableDelegate {
    pub store: ResultStore,
    pub view: TableView,
    pub visible: Vec<usize>,
    pub columns: Vec<Column>,
    pub workspace: WeakEntity<Workspace>,
    pub inspect: bool,
    pub font_px: f32,
    pub expand_prefix: String,
}

impl ResultTableDelegate {
    pub fn empty(workspace: WeakEntity<Workspace>) -> Self {
        Self {
            store: ResultStore::default(),
            view: TableView::default(),
            visible: Vec::new(),
            columns: vec![index_column()],
            workspace,
            inspect: false,
            font_px: 12.0,
            expand_prefix: String::new(),
        }
    }

    pub fn bind(&mut self, store: ResultStore, view: TableView) {
        self.visible = store.visible_indices(&view);
        self.columns = build_columns(&store);
        self.store = store;
        self.view = view;
    }

    pub fn set_font_px(&mut self, font_px: f32) {
        self.font_px = font_px;
    }

    fn cell_at(&self, row_ix: usize, col_ix: usize) -> Option<&Cell> {
        let data_col = col_ix.checked_sub(1)?;
        let store_row = *self.visible.get(row_ix)?;
        self.store.rows.get(store_row)?.get(data_col)
    }
}

fn index_column() -> Column {
    Column::new("#", "#")
        .width(px(48.))
        .fixed(ColumnFixed::Left)
        .resizable(false)
        .movable(false)
        .sortable()
}

fn build_columns(store: &ResultStore) -> Vec<Column> {
    let mut cols = vec![index_column()];
    for (ix, name) in store.columns.iter().enumerate() {
        let mut col = Column::new(name.clone(), name.clone())
            .width(px(store.column_px(ix)))
            .min_width(px(72.))
            .sortable();
        if matches!(name.as_str(), "size" | "type") {
            col = col.text_right();
        }
        cols.push(col);
    }
    cols
}

impl TableDelegate for ResultTableDelegate {
    fn columns_count(&self, _: &App) -> usize {
        self.columns.len().max(1)
    }

    fn rows_count(&self, _: &App) -> usize {
        self.visible.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        self.columns
            .get(col_ix)
            .cloned()
            .unwrap_or_else(|| Column::new("?", "?"))
    }

    fn perform_sort(
        &mut self,
        col_ix: usize,
        sort: ColumnSort,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) {
        if col_ix == 0 {
            return;
        }
        let data_col = col_ix - 1;
        self.view.sort_col = Some(data_col);
        self.view.sort_asc = !matches!(sort, ColumnSort::Descending);
        self.visible = self.store.visible_indices(&self.view);
        cx.notify();
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let font_px = self.font_px;
        if col_ix == 0 {
            return div()
                .w_full()
                .h_full()
                .flex()
                .items_center()
                .justify_end()
                .px_2()
                .text_size(px(font_px))
                .text_color(cx.theme().info)
                .child((row_ix + 1).to_string())
                .into_any_element();
        }

        let Some(cell) = self.cell_at(row_ix, col_ix).cloned() else {
            return div().into_any_element();
        };

        if cell.nested.is_some() {
            let workspace = self.workspace.clone();
            let store_row = self.visible.get(row_ix).copied().unwrap_or(row_ix);
            let data_col = col_ix - 1;
            let prefix = if self.expand_prefix.is_empty() {
                "root"
            } else {
                self.expand_prefix.as_str()
            };
            let expand_key = format!("{prefix}/{store_row}/{data_col}");
            let expanded = self
                .workspace
                .upgrade()
                .is_some_and(|entity| entity.read(cx).is_inline_expanded(&expand_key));
            let inspect = self.inspect;
            return wrap_resizable_td(
                div()
                    .id(SharedString::from(format!(
                        "{}-nested-{row_ix}-{col_ix}",
                        if inspect { "i" } else { "r" }
                    )))
                    .h(px(font_px + 8.))
                    .px_1p5()
                    .rounded(px(3.))
                    .border_1()
                    .border_color(cx.theme().border)
                    .bg(cx.theme().secondary)
                    .flex()
                    .items_center()
                    .cursor_pointer()
                    .hover(|this| this.bg(cx.theme().secondary.opacity(0.75)))
                    .on_click(move |_, _, cx| {
                        let key = expand_key.clone();
                        let _ = workspace.update(cx, |this, cx| {
                            this.toggle_inline_expand(key, cx);
                        });
                    })
                    .child(
                        div()
                            .text_size(px(font_px))
                            .font_family(cx.theme().mono_font_family.clone())
                            .text_color(cx.theme().info)
                            .child(format!(
                                "{} {}",
                                if expanded { "▾" } else { "▸" },
                                cell.expand_label()
                            )),
                    ),
                self.workspace.clone(),
                row_ix,
                col_ix - 1,
                cx,
            );
        }

        let dark = cx.theme().mode.is_dark();
        if looks_like_ansi(&cell.text) && !cell.text.trim().is_empty() {
            let fallback = cx.theme().foreground;
            return wrap_resizable_td(
                div()
                    .w_full()
                    .h_full()
                    .flex()
                    .items_center()
                    .px_2()
                    .children(
                        parse_ansi_lines(&cell.text)
                            .into_iter()
                            .flatten()
                            .map(|span| {
                                let fg = span
                                    .fg
                                    .map(|rgb| rgb.contrast_on(dark).hsla())
                                    .unwrap_or(fallback);
                                div()
                                    .text_size(px(font_px))
                                    .font_family(cx.theme().mono_font_family.clone())
                                    .when_some(span.bg, |this, bg| {
                                        this.bg(bg.contrast_on(dark).hsla())
                                    })
                                    .text_color(fg)
                                    .font_weight(if span.bold {
                                        FontWeight::SEMIBOLD
                                    } else {
                                        FontWeight::NORMAL
                                    })
                                    .child(span.text)
                            }),
                    ),
                self.workspace.clone(),
                row_ix,
                col_ix - 1,
                cx,
            );
        }

        let color = cell
            .color
            .map(|(r, g, b)| {
                Rgb {
                    r,
                    g,
                    b,
                    bold: cell.bold,
                }
                .contrast_on(dark)
                .hsla()
            })
            .unwrap_or(cx.theme().foreground);

        wrap_resizable_td(
            div()
                .w_full()
                .h_full()
                .flex()
                .items_center()
                .px_2()
                .text_size(px(font_px))
                .font_family(cx.theme().mono_font_family.clone())
                .font_weight(if cell.bold {
                    FontWeight::SEMIBOLD
                } else {
                    FontWeight::NORMAL
                })
                .text_color(color)
                .when(cell.kind.is_numeric(), |this| this.justify_end())
                .child(cell.text),
            self.workspace.clone(),
            row_ix,
            col_ix - 1,
            cx,
        )
    }
}

fn wrap_resizable_td(
    content: impl IntoElement,
    workspace: WeakEntity<Workspace>,
    row_ix: usize,
    data_col: usize,
    cx: &mut Context<TableState<ResultTableDelegate>>,
) -> gpui::AnyElement {
    let origin_ws = workspace.clone();
    div()
        .id(SharedString::from(format!("td-wrap-{row_ix}-{data_col}")))
        .relative()
        .size_full()
        .child(content)
        .child(
            canvas(
                move |bounds, _, cx| {
                    let _ = origin_ws.update(cx, |this, _| {
                        this.note_table_origin(
                            &format!("virtual:{data_col}"),
                            f32::from(bounds.origin.x),
                        );
                    });
                },
                |_, _, _, _| {},
            )
            .absolute()
            .top_0()
            .left_0()
            .w_full()
            .h(px(1.)),
        )
        .child(virtual_col_resize_handle(workspace, row_ix, data_col, cx))
        .into_any_element()
}

fn virtual_col_resize_handle(
    workspace: WeakEntity<Workspace>,
    row_ix: usize,
    data_col: usize,
    cx: &mut Context<TableState<ResultTableDelegate>>,
) -> impl IntoElement {
    let ws_move = workspace.clone();
    div()
        .id(SharedString::from(format!("v-resize-{row_ix}-{data_col}")))
        .absolute()
        .top_0()
        .right_0()
        .h_full()
        .w(px(8.))
        .occlude()
        .cursor_col_resize()
        .flex()
        .justify_end()
        .child(div().w(px(1.)).h_full().bg(cx.theme().border.opacity(0.45)))
        .on_drag(
            ResizeInspectCol {
                path: "virtual".into(),
                col: data_col,
                left_offset: 0.0,
            },
            |drag, _, _, cx| {
                cx.stop_propagation();
                cx.new(|_| drag.clone())
            },
        )
        .on_drag_move(move |e: &DragMoveEvent<ResizeInspectCol>, _, cx| {
            let spec = e.drag(cx).clone();
            if spec.path != "virtual" || spec.col != data_col {
                return;
            }
            let mouse_x = f32::from(e.event.position.x);
            let _ = ws_move.update(cx, |this, cx| {
                this.drag_resize_column(&spec, mouse_x, cx);
            });
        })
}
