//! Results produced by an Invocation.

use std::cmp::Ordering;
use std::time::Duration;

use crate::ansi::{looks_like_ansi, visible_text};
use crate::expand::{cell_display_width, cell_line_count};

/// Identifier for a Result in the Session stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ResultId(pub u64);

/// Lifecycle of a Result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultStatus {
    Running,
    Succeeded,
    Failed,
    Interrupted,
}

/// How a cell should be colored, aligned, and sorted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CellKind {
    #[default]
    Text,
    Int,
    Float,
    Bool,
    Filesize,
    Duration,
    Date,
    Empty,
    Other,
}

impl CellKind {
    pub fn is_numeric(self) -> bool {
        matches!(
            self,
            Self::Int | Self::Float | Self::Filesize | Self::Duration
        )
    }
}

/// Typed sort payload so display text never drives order.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum SortKey {
    #[default]
    Empty,
    Text(String),
    Int(i64),
    Float(f64),
}

impl Eq for SortKey {}

impl PartialOrd for SortKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SortKey {
    fn cmp(&self, other: &Self) -> Ordering {
        use SortKey::*;
        match (self, other) {
            (Empty, Empty) => Ordering::Equal,
            (Empty, _) => Ordering::Less,
            (_, Empty) => Ordering::Greater,
            (Int(a), Int(b)) => a.cmp(b),
            (Float(a), Float(b)) => a.total_cmp(b),
            (Int(a), Float(b)) => (*a as f64).total_cmp(b),
            (Float(a), Int(b)) => a.total_cmp(&(*b as f64)),
            (Text(a), Text(b)) => a.to_lowercase().cmp(&b.to_lowercase()),
            (Int(a), Text(b)) => a.to_string().cmp(b),
            (Float(a), Text(b)) => a.to_string().cmp(b),
            (Text(a), Int(b)) => a.cmp(&b.to_string()),
            (Text(a), Float(b)) => a.cmp(&b.to_string()),
        }
    }
}

/// One display cell.
#[derive(Debug, Clone, Default)]
pub struct Cell {
    pub text: String,
    pub kind: CellKind,
    pub sort: SortKey,
    pub color: Option<(u8, u8, u8)>,
    pub bold: bool,
    pub nested: Option<Box<ResultStore>>,
}

impl Cell {
    pub fn text(text: impl Into<String>) -> Self {
        let text = text.into();
        Self {
            sort: SortKey::Text(text.to_lowercase()),
            text,
            kind: CellKind::Text,
            color: None,
            bold: false,
            nested: None,
        }
    }

    pub fn is_nested(&self) -> bool {
        self.nested.is_some()
    }

    pub fn line_count(&self) -> usize {
        let n = self.text.lines().count();
        if n == 0 && !self.text.is_empty() {
            1
        } else {
            n
        }
    }

    pub fn is_multiline(&self) -> bool {
        self.text.contains('\n')
    }

    pub fn expand_label(&self) -> String {
        if self
            .nested
            .as_deref()
            .is_some_and(ResultStore::is_text_block)
        {
            let n = self.line_count();
            if n == 1 {
                "1 line".into()
            } else {
                format!("{n} lines")
            }
        } else {
            self.text.clone()
        }
    }

    pub fn export_text(&self) -> &str {
        self.text.as_str()
    }
}

/// Compact display store for a table Result body.
#[derive(Debug, Clone, Default)]
pub struct ResultStore {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Cell>>,
}

/// View-only sort/filter on a Result store.
#[derive(Debug, Clone, Default)]
pub struct TableView {
    pub sort_col: Option<usize>,
    pub sort_asc: bool,
    /// Exact-match filter per column (`None` = no column filter).
    pub equals: Vec<Option<String>>,
    /// Substring search across all columns.
    pub search: String,
}

impl TableView {
    pub fn for_columns(n: usize) -> Self {
        Self {
            sort_col: None,
            sort_asc: true,
            equals: vec![None; n],
            search: String::new(),
        }
    }

    pub fn cycle_sort(&mut self, col: usize) {
        match self.sort_col {
            Some(current) if current == col => {
                if self.sort_asc {
                    self.sort_asc = false;
                } else {
                    self.sort_col = None;
                    self.sort_asc = true;
                }
            }
            _ => {
                self.sort_col = Some(col);
                self.sort_asc = true;
            }
        }
    }
}

impl ResultStore {
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    pub fn is_pairs(&self) -> bool {
        self.columns.len() == 2 && self.columns[0] == "field" && self.columns[1] == "value"
    }

    pub fn can_inline(&self) -> bool {
        !self.rows.is_empty() && self.row_count() <= 12 && self.columns.len() <= 8
    }

    pub fn has_nested(&self) -> bool {
        self.rows.iter().any(|row| row.iter().any(Cell::is_nested))
    }

    /// Large list-of-records tables must stay virtualized; pairs stay custom.
    pub fn prefers_virtual(&self) -> bool {
        !self.is_pairs() && self.row_count() > 64
    }

    pub fn text_block(text: impl Into<String>) -> Self {
        Self {
            columns: vec!["text".into()],
            rows: vec![vec![Cell::text(text)]],
        }
    }

    pub fn is_text_block(&self) -> bool {
        self.columns == ["text".to_string()]
            && self.rows.len() == 1
            && self
                .rows
                .first()
                .and_then(|row| row.first())
                .is_some_and(|cell| cell.nested.is_none())
    }

    /// Pixel width for a column from its content, never flex-squeezed.
    pub fn column_px(&self, col: usize) -> f32 {
        let header = self
            .columns
            .get(col)
            .map(|name| cell_display_width(name) + 4)
            .unwrap_or(8);
        let body = self
            .rows
            .iter()
            .filter_map(|row| row.get(col))
            .map(|cell| {
                let text = if looks_like_ansi(&cell.text) {
                    visible_text(&cell.text)
                } else {
                    cell.text.clone()
                };
                cell_display_width(&text)
            })
            .max()
            .unwrap_or(0);
        let nested = self
            .rows
            .iter()
            .any(|row| row.get(col).is_some_and(|cell| cell.nested.is_some()));
        let chars = header.max(body).max(6);
        let min = if nested { 128.0 } else { 88.0 };
        ((chars as f32) * 7.6 + 28.0).clamp(min, 640.0)
    }

    pub fn table_px(&self) -> f32 {
        44.0 + (0..self.columns.len())
            .map(|col| self.column_px(col))
            .sum::<f32>()
    }

    pub fn row_line_count(&self, row: usize) -> usize {
        self.rows
            .get(row)
            .map(|r| {
                r.iter()
                    .map(|cell| cell_line_count(&cell.text))
                    .max()
                    .unwrap_or(1)
            })
            .unwrap_or(1)
            .clamp(1, 18)
    }

    pub fn visible_indices(&self, view: &TableView) -> Vec<usize> {
        let search = view.search.trim().to_lowercase();
        let mut indices: Vec<usize> = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row_matches(row, view, &search))
            .map(|(ix, _)| ix)
            .collect();

        if let Some(col) = view.sort_col {
            indices.sort_by(|&a, &b| {
                let left = self.rows.get(a).and_then(|r| r.get(col));
                let right = self.rows.get(b).and_then(|r| r.get(col));
                let ord = cmp_cells(left, right);
                if view.sort_asc { ord } else { ord.reverse() }
            });
        }
        indices
    }

    pub fn unique_values(&self, col: usize, limit: usize) -> Vec<String> {
        let mut values = Vec::new();
        for row in &self.rows {
            if let Some(cell) = row.get(col) {
                if !cell.text.is_empty() && !values.iter().any(|v| v == &cell.text) {
                    values.push(cell.text.clone());
                }
            }
            if values.len() >= limit {
                break;
            }
        }
        values.sort();
        values
    }

    pub fn to_delimited(&self, indices: &[usize], delimiter: char) -> String {
        let mut out = self.columns.join(&delimiter.to_string());
        out.push('\n');
        for &ix in indices {
            let Some(row) = self.rows.get(ix) else {
                continue;
            };
            let line: Vec<String> = (0..self.columns.len())
                .map(|col| {
                    escape_delimited(row.get(col).map(Cell::export_text).unwrap_or(""), delimiter)
                })
                .collect();
            out.push_str(&line.join(&delimiter.to_string()));
            out.push('\n');
        }
        out
    }

    pub fn to_json(&self, indices: &[usize]) -> String {
        let mut rows = Vec::with_capacity(indices.len());
        for &ix in indices {
            let Some(row) = self.rows.get(ix) else {
                continue;
            };
            let mut obj = String::from("{");
            for (i, col) in self.columns.iter().enumerate() {
                if i > 0 {
                    obj.push_str(", ");
                }
                obj.push_str(&format!(
                    "\"{}\": {}",
                    escape_json(col),
                    json_value(row.get(i).map(Cell::export_text).unwrap_or(""))
                ));
            }
            obj.push('}');
            rows.push(obj);
        }
        format!("[\n  {}\n]", rows.join(",\n  "))
    }

    pub fn to_nuon(&self, indices: &[usize]) -> String {
        let mut rows = Vec::with_capacity(indices.len());
        for &ix in indices {
            let Some(row) = self.rows.get(ix) else {
                continue;
            };
            let fields: Vec<String> = self
                .columns
                .iter()
                .enumerate()
                .map(|(i, col)| {
                    format!(
                        "{}: {}",
                        nuon_key(col),
                        nuon_value(row.get(i).map(Cell::export_text).unwrap_or(""))
                    )
                })
                .collect();
            rows.push(format!("{{{}}}", fields.join(", ")));
        }
        format!("[{}]", rows.join(", "))
    }

    pub fn to_markdown(&self, indices: &[usize]) -> String {
        let mut out = format!("| {} |\n", self.columns.join(" | "));
        out.push_str(&format!(
            "| {} |\n",
            self.columns
                .iter()
                .map(|_| "---")
                .collect::<Vec<_>>()
                .join(" | ")
        ));
        for &ix in indices {
            let Some(row) = self.rows.get(ix) else {
                continue;
            };
            let cells: Vec<String> = (0..self.columns.len())
                .map(|col| {
                    row.get(col)
                        .map(|c| c.export_text().replace('\n', " "))
                        .unwrap_or_default()
                })
                .collect();
            out.push_str(&format!("| {} |\n", cells.join(" | ")));
        }
        out
    }

    pub fn to_plain_text(&self, indices: &[usize]) -> String {
        if self.is_text_block() {
            return self
                .rows
                .first()
                .and_then(|row| row.first())
                .map(|cell| cell.text.clone())
                .unwrap_or_default();
        }
        let mut out = self.columns.join("\t");
        out.push('\n');
        for &ix in indices {
            let Some(row) = self.rows.get(ix) else {
                continue;
            };
            let line: Vec<String> = (0..self.columns.len())
                .map(|col| {
                    row.get(col)
                        .map(|cell| cell.export_text().replace('\n', " "))
                        .unwrap_or_default()
                })
                .collect();
            out.push_str(&line.join("\t"));
            out.push('\n');
        }
        out
    }
}

fn row_matches(row: &[Cell], view: &TableView, search: &str) -> bool {
    for (col, expected) in view.equals.iter().enumerate() {
        if let Some(expected) = expected {
            let actual = row.get(col).map(|c| c.text.as_str()).unwrap_or("");
            if actual != expected {
                return false;
            }
        }
    }
    if search.is_empty() {
        return true;
    }
    row.iter()
        .any(|cell| cell.text.to_lowercase().contains(search))
}

fn cmp_cells(left: Option<&Cell>, right: Option<&Cell>) -> Ordering {
    match (left, right) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Less,
        (Some(_), None) => Ordering::Greater,
        (Some(a), Some(b)) => a.sort.cmp(&b.sort),
    }
}

fn escape_delimited(text: &str, delimiter: char) -> String {
    if text.contains(delimiter) || text.contains('"') || text.contains('\n') {
        format!("\"{}\"", text.replace('"', "\"\""))
    } else {
        text.to_string()
    }
}

fn escape_json(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
}

fn json_value(text: &str) -> String {
    if text.parse::<f64>().is_ok() && !text.is_empty() {
        text.to_string()
    } else if text == "true" || text == "false" {
        text.to_string()
    } else {
        format!("\"{}\"", escape_json(text))
    }
}

fn nuon_key(text: &str) -> String {
    if text
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        text.to_string()
    } else {
        format!("\"{}\"", escape_json(text))
    }
}

fn nuon_value(text: &str) -> String {
    json_value(text)
}

/// Typed view inside a Result.
#[derive(Debug, Clone)]
pub enum ResultBody {
    Table(ResultStore),
    Scalar(String),
    Text(String),
    Diagnostic(String),
    Binary { bytes: usize, summary: String },
    Empty,
}

/// One Invocation's UI object.
#[derive(Debug, Clone)]
pub struct InvocationResult {
    pub id: ResultId,
    pub source: String,
    pub status: ResultStatus,
    pub duration: Option<Duration>,
    pub exit_code: Option<i64>,
    pub body: ResultBody,
    pub log: Vec<String>,
    pub expanded: bool,
    pub view: TableView,
}

impl InvocationResult {
    pub fn running(id: ResultId, source: impl Into<String>) -> Self {
        Self {
            id,
            source: source.into(),
            status: ResultStatus::Running,
            duration: None,
            exit_code: None,
            body: ResultBody::Empty,
            log: Vec::new(),
            expanded: true,
            view: TableView::default(),
        }
    }

    pub fn status_label(&self) -> &'static str {
        match self.status {
            ResultStatus::Running => "running",
            ResultStatus::Succeeded => "ok",
            ResultStatus::Failed => "error",
            ResultStatus::Interrupted => "interrupted",
        }
    }
}

/// Clipboard payload for a Result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyFormat {
    Tsv,
    Csv,
    Json,
    Nuon,
    Markdown,
    Text,
}

impl CopyFormat {
    pub fn label(self) -> &'static str {
        match self {
            Self::Tsv => "TSV",
            Self::Csv => "CSV",
            Self::Json => "JSON",
            Self::Nuon => "NUON",
            Self::Markdown => "Markdown",
            Self::Text => "Text",
        }
    }
}

pub fn copy_result(body: &ResultBody, view: &TableView, format: CopyFormat) -> String {
    match body {
        ResultBody::Table(store) => {
            let indices = store.visible_indices(view);
            match format {
                CopyFormat::Tsv => store.to_delimited(&indices, '\t'),
                CopyFormat::Csv => store.to_delimited(&indices, ','),
                CopyFormat::Json => store.to_json(&indices),
                CopyFormat::Nuon => store.to_nuon(&indices),
                CopyFormat::Markdown => store.to_markdown(&indices),
                CopyFormat::Text => store.to_plain_text(&indices),
            }
        }
        ResultBody::Scalar(text) | ResultBody::Text(text) | ResultBody::Diagnostic(text) => {
            text.clone()
        }
        ResultBody::Binary { summary, .. } => summary.clone(),
        ResultBody::Empty => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> ResultStore {
        ResultStore {
            columns: vec!["name".into(), "size".into()],
            rows: vec![
                vec![
                    Cell::text("b"),
                    Cell {
                        text: "20 B".into(),
                        kind: CellKind::Filesize,
                        sort: SortKey::Int(20),
                        color: None,
                        bold: false,
                        nested: None,
                    },
                ],
                vec![
                    Cell::text("a"),
                    Cell {
                        text: "3 B".into(),
                        kind: CellKind::Filesize,
                        sort: SortKey::Int(3),
                        color: None,
                        bold: false,
                        nested: None,
                    },
                ],
                vec![
                    Cell::text("c"),
                    Cell {
                        text: "10 B".into(),
                        kind: CellKind::Filesize,
                        sort: SortKey::Int(10),
                        color: None,
                        bold: false,
                        nested: None,
                    },
                ],
            ],
        }
    }

    #[test]
    fn sort_numeric_column() {
        let store = store();
        let mut view = TableView::for_columns(2);
        view.sort_col = Some(1);
        view.sort_asc = true;
        let indices = store.visible_indices(&view);
        let sizes: Vec<_> = indices
            .iter()
            .map(|&i| store.rows[i][1].text.as_str())
            .collect();
        assert_eq!(sizes, ["3 B", "10 B", "20 B"]);
    }

    #[test]
    fn filter_equals_and_search() {
        let store = store();
        let mut view = TableView::for_columns(2);
        view.equals[0] = Some("a".into());
        assert_eq!(store.visible_indices(&view), vec![1]);
        view.equals[0] = None;
        view.search = "c".into();
        assert_eq!(store.visible_indices(&view), vec![2]);
    }

    #[test]
    fn has_nested_detects_child_store() {
        let mut cell = Cell::text("13 rows");
        cell.nested = Some(Box::new(ResultStore {
            columns: vec!["value".into()],
            rows: vec![vec![Cell::text("a")]],
        }));
        let nested = ResultStore {
            columns: vec!["field".into(), "value".into()],
            rows: vec![vec![Cell::text("menus"), cell]],
        };
        assert!(nested.has_nested());
        assert!(!store().has_nested());
        assert!(!nested.prefers_virtual());
        let mut large = store();
        large.rows = (0..80)
            .map(|_| vec![Cell::text("a"), Cell::text("b")])
            .collect();
        assert!(large.prefers_virtual());
    }

    #[test]
    fn multiline_cell_keeps_text_for_search_and_labels_lines() {
        let mut cell = Cell::text("first\nsecond\nthird");
        cell.nested = Some(Box::new(ResultStore::text_block(cell.text.clone())));
        assert_eq!(cell.expand_label(), "3 lines");
        assert!(cell.text.contains("second"));
        let store = ResultStore {
            columns: vec!["extra_description".into()],
            rows: vec![vec![cell]],
        };
        let mut view = TableView::for_columns(1);
        view.search = "second".into();
        assert_eq!(store.visible_indices(&view), vec![0]);
        assert!(store.to_plain_text(&[0]).contains("first second third"));
    }

    #[test]
    fn text_block_copy_returns_raw_text() {
        let store = ResultStore::text_block("a\nb\nc");
        assert!(store.is_text_block());
        assert_eq!(store.to_plain_text(&[0]), "a\nb\nc");
    }

    #[test]
    fn csv_export_quotes_commas() {
        let store = ResultStore {
            columns: vec!["msg".into()],
            rows: vec![vec![Cell::text("hello, world")]],
        };
        let csv = store.to_delimited(&[0], ',');
        assert!(csv.contains("\"hello, world\""));
    }
}
