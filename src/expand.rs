//! Expand nested records and lists the way `table -e` does in the REPL.

use nu_protocol::{Config, Value};

const MAX_DEPTH: usize = 3;
const MAX_INNER_ROWS: usize = 24;

/// Pretty-print a value for a table cell, expanding nested tables/records.
pub fn format_expanded(value: &Value, config: &Config) -> String {
    format_depth(value, config, MAX_DEPTH)
}

fn format_depth(value: &Value, config: &Config, depth: usize) -> String {
    if depth == 0 {
        return value.to_abbreviated_string(config);
    }
    match value {
        Value::Record { val, .. } if !val.is_empty() => format_record(val, config, depth),
        Value::List { vals, .. } if vals.is_empty() => "[]".into(),
        Value::List { vals, .. } if vals.iter().all(|v| matches!(v, Value::Record { .. })) => {
            format_record_list(vals, config, depth)
        }
        Value::List { vals, .. } => format_scalar_list(vals, config, depth),
        other => format_primitive(other, config),
    }
}

fn format_primitive(value: &Value, config: &Config) -> String {
    match value {
        Value::Nothing { .. } => String::new(),
        Value::String { val, .. } => val.clone(),
        Value::Int { val, .. } => val.to_string(),
        Value::Float { val, .. } => {
            format!(
                "{val:.prec$}",
                prec = config.float_precision.max(0) as usize
            )
        }
        Value::Bool { val, .. } => val.to_string(),
        Value::Filesize { val, .. } => config.filesize.format(*val).to_string(),
        Value::Date { .. } | Value::Duration { .. } => value.to_abbreviated_string(config),
        other => other.to_expanded_string(", ", config),
    }
}

fn format_record(record: &nu_protocol::Record, config: &Config, depth: usize) -> String {
    let keys: Vec<&str> = record.columns().map(String::as_str).collect();
    let key_w = keys.iter().map(|k| display_width(k)).max().unwrap_or(0);
    record
        .iter()
        .map(|(key, value)| {
            let rendered = format_depth(value, config, depth - 1);
            let mut lines = rendered.lines();
            let first = lines.next().unwrap_or("");
            let mut out = format!("{key:<key_w$}  {first}");
            for line in lines {
                out.push('\n');
                out.push_str(&" ".repeat(key_w + 2));
                out.push_str(line);
            }
            out
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_record_list(vals: &[Value], config: &Config, depth: usize) -> String {
    let mut columns: Vec<String> = Vec::new();
    for val in vals.iter().take(MAX_INNER_ROWS) {
        if let Value::Record { val, .. } = val {
            for col in val.columns() {
                if !columns.iter().any(|c| c == col) {
                    columns.push(col.clone());
                }
            }
        }
    }
    if columns.is_empty() {
        return format!("[{} rows]", vals.len());
    }

    let mut rows: Vec<Vec<String>> = Vec::new();
    for val in vals.iter().take(MAX_INNER_ROWS) {
        let Value::Record { val, .. } = val else {
            continue;
        };
        rows.push(
            columns
                .iter()
                .map(|col| {
                    val.get(col)
                        .map(|v| format_depth(v, config, depth - 1))
                        .unwrap_or_default()
                })
                .collect(),
        );
    }

    let widths: Vec<usize> = columns
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let header = display_width(name);
            let body = rows
                .iter()
                .map(|row| {
                    row.get(i)
                        .map(|cell| cell.lines().map(display_width).max().unwrap_or(0))
                        .unwrap_or(0)
                })
                .max()
                .unwrap_or(0);
            header.max(body).min(48).max(name.len())
        })
        .collect();

    let mut lines = Vec::new();
    lines.push(format_row(
        &columns.iter().map(String::as_str).collect::<Vec<_>>(),
        &widths,
    ));
    lines.push(
        widths
            .iter()
            .map(|w| "-".repeat(*w))
            .collect::<Vec<_>>()
            .join("  "),
    );
    for row in &rows {
        let first_lines: Vec<String> = row
            .iter()
            .map(|cell| cell.lines().next().unwrap_or("").to_string())
            .collect();
        lines.push(format_row(
            &first_lines.iter().map(String::as_str).collect::<Vec<_>>(),
            &widths,
        ));
        let extra = row
            .iter()
            .map(|c| c.lines().count().saturating_sub(1))
            .max()
            .unwrap_or(0);
        for extra_ix in 0..extra {
            let parts: Vec<String> = row
                .iter()
                .map(|cell| cell.lines().nth(extra_ix + 1).unwrap_or("").to_string())
                .collect();
            lines.push(format_row(
                &parts.iter().map(String::as_str).collect::<Vec<_>>(),
                &widths,
            ));
        }
    }
    if vals.len() > MAX_INNER_ROWS {
        lines.push(format!("… {} more rows", vals.len() - MAX_INNER_ROWS));
    }
    lines.join("\n")
}

fn format_scalar_list(vals: &[Value], config: &Config, depth: usize) -> String {
    vals.iter()
        .take(MAX_INNER_ROWS)
        .map(|v| format!("• {}", format_depth(v, config, depth - 1)))
        .chain(
            (vals.len() > MAX_INNER_ROWS)
                .then(|| format!("• … {} more", vals.len() - MAX_INNER_ROWS)),
        )
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_row(cells: &[&str], widths: &[usize]) -> String {
    cells
        .iter()
        .zip(widths)
        .map(|(cell, width)| format!("{cell:<width$}"))
        .collect::<Vec<_>>()
        .join("  ")
}

pub fn display_width(text: &str) -> usize {
    text.chars().map(|c| if c.is_ascii() { 1 } else { 2 }).sum()
}

pub fn cell_display_width(text: &str) -> usize {
    text.lines().map(display_width).max().unwrap_or(0)
}

pub fn cell_line_count(text: &str) -> usize {
    text.lines().count().max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nu_protocol::{Record, Span, record};

    #[test]
    fn expands_record_list_as_inner_table() {
        let config = Config::default();
        let rows = vec![
            Value::record(
                record! { "name" => Value::string("ls", Span::test_data()), "type" => Value::string("plugin", Span::test_data()) },
                Span::test_data(),
            ),
            Value::record(
                record! { "name" => Value::string("open", Span::test_data()), "type" => Value::string("builtin", Span::test_data()) },
                Span::test_data(),
            ),
        ];
        let text = format_expanded(&Value::list(rows, Span::test_data()), &config);
        assert!(text.contains("name"));
        assert!(text.contains("ls"));
        assert!(text.contains("open"));
        assert!(!text.contains("[table"));
    }

    #[test]
    fn expands_record_as_pairs() {
        let config = Config::default();
        let rec = Record::from_iter([
            ("input".into(), Value::string("nothing", Span::test_data())),
            ("output".into(), Value::string("table", Span::test_data())),
        ]);
        let text = format_expanded(&Value::record(rec, Span::test_data()), &config);
        assert!(text.contains("input"));
        assert!(text.contains("output"));
        assert!(!text.contains("[record"));
    }
}
