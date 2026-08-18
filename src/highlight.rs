//! Nushell-shaped syntax highlighting for the REPL editor.

use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;

use gpui::{Context, FontWeight, HighlightStyle, SharedString, Window};
use gpui_component::input::{
    EditorState, FoldRange, HighlightStyleResolver, InputEdit, InputHighlighter,
    InputHighlighterFactory, Rope,
};
use nu_cmd_lang::create_default_context;
use nu_color_config::get_shape_color;
use nu_command::add_shell_command_context;
use nu_parser::{flatten_block, parse};
use nu_protocol::Config;
use nu_protocol::engine::{EngineState, StateWorkingSet};

use crate::color::Rgb;

pub fn nu_highlighter_factory() -> InputHighlighterFactory {
    Rc::new(|language| {
        if matches!(language, "nushell" | "nu") {
            Some(Box::new(NuInputHighlighter::new()))
        } else {
            None
        }
    })
}

pub struct NuInputHighlighter {
    engine: EngineState,
    config: Config,
    runs: Vec<(Range<usize>, HighlightStyle)>,
}

impl NuInputHighlighter {
    pub fn new() -> Self {
        let mut engine = create_default_context();
        engine = add_shell_command_context(engine);
        Self {
            engine,
            config: Config::default(),
            runs: Vec::new(),
        }
    }
}

impl InputHighlighter for NuInputHighlighter {
    fn language(&self) -> SharedString {
        "nushell".into()
    }

    fn update(
        &mut self,
        _: Option<InputEdit>,
        text: &Rope,
        _: bool,
        _: &mut Window,
        _: &mut Context<EditorState>,
    ) {
        let line = text.to_string();
        self.runs = highlight_runs(&self.engine, &self.config, &line, true);
    }

    fn styles(
        &self,
        range: &Range<usize>,
        _: &dyn HighlightStyleResolver,
    ) -> Vec<(Range<usize>, HighlightStyle)> {
        let mut out = Vec::new();
        let mut pos = range.start;
        for (run, style) in &self.runs {
            if run.end <= range.start {
                continue;
            }
            if run.start >= range.end {
                break;
            }
            let start = run.start.max(range.start);
            let end = run.end.min(range.end);
            if start > pos {
                out.push((pos..start, HighlightStyle::default()));
            }
            if start < end {
                out.push((start..end, *style));
                pos = end;
            }
        }
        if pos < range.end {
            out.push((pos..range.end, HighlightStyle::default()));
        }
        if out.is_empty() {
            out.push((range.clone(), HighlightStyle::default()));
        }
        out
    }

    fn fold_ranges(&self, _: &Rope) -> Vec<FoldRange> {
        Vec::new()
    }
}

/// Highlight `source` into display lines of (text, style) runs.
pub fn highlight_lines(source: &str, dark: bool) -> Vec<Vec<(String, HighlightStyle)>> {
    thread_local! {
        static ENGINE: RefCell<Option<(EngineState, Config)>> = const { RefCell::new(None) };
    }
    ENGINE.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            let mut engine = create_default_context();
            engine = add_shell_command_context(engine);
            *slot = Some((engine, Config::default()));
        }
        let (engine, config) = slot.as_ref().expect("highlighter engine");
        let runs = highlight_runs(engine, config, source, dark);
        split_runs_into_lines(source, &runs)
    })
}

fn split_runs_into_lines(
    source: &str,
    runs: &[(Range<usize>, HighlightStyle)],
) -> Vec<Vec<(String, HighlightStyle)>> {
    let mut lines: Vec<Vec<(String, HighlightStyle)>> = vec![Vec::new()];
    for (range, style) in runs {
        let Some(text) = source.get(range.start..range.end) else {
            continue;
        };
        for (i, part) in text.split('\n').enumerate() {
            if i > 0 {
                lines.push(Vec::new());
            }
            if !part.is_empty() {
                lines
                    .last_mut()
                    .expect("line")
                    .push((part.to_string(), *style));
            }
        }
    }
    if lines.len() == 1 && lines[0].is_empty() && !source.is_empty() {
        lines[0].push((source.to_string(), HighlightStyle::default()));
    }
    lines
}

fn highlight_runs(
    engine: &EngineState,
    config: &Config,
    line: &str,
    dark: bool,
) -> Vec<(Range<usize>, HighlightStyle)> {
    if line.is_empty() {
        return Vec::new();
    }
    let mut working_set = StateWorkingSet::new(engine);
    let block = parse(&mut working_set, None, line.as_bytes(), false);
    let shapes = flatten_block(&working_set, &block);
    let offset = engine.next_span_start();
    let mut runs = Vec::new();
    let mut last = 0usize;

    for (span, shape) in shapes {
        if span.end <= offset || span.start < offset {
            continue;
        }
        let start = span.start - offset;
        let end = span.end - offset;
        if end <= last || start >= line.len() {
            continue;
        }
        let start = start.max(last).min(line.len());
        let end = end.min(line.len());
        if start >= end {
            continue;
        }
        if start > last {
            runs.push((last..start, HighlightStyle::default()));
        }
        runs.push((start..end, style_for_shape(shape.as_str(), config, dark)));
        last = end;
    }
    if last < line.len() {
        runs.push((last..line.len(), HighlightStyle::default()));
    }
    runs
}

fn style_for_shape(shape: &str, config: &Config, dark: bool) -> HighlightStyle {
    let style = get_shape_color(shape, config);
    let color = Rgb::from_style(style).map(|rgb| rgb.contrast_on(dark).hsla());
    HighlightStyle {
        color,
        font_weight: style.is_bold.then_some(FontWeight::SEMIBOLD),
        ..HighlightStyle::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlights_pipeline_tokens() {
        let hl = NuInputHighlighter::new();
        let runs = highlight_runs(&hl.engine, &hl.config, "ls | where type == file", true);
        let lines = highlight_lines("def foo [] { 1 }", true);
        assert!(!lines.is_empty());
        assert!(
            runs.len() >= 3,
            "expected several highlight runs, got {runs:?}"
        );
        assert!(
            runs.iter()
                .any(|(range, style)| { range.start < 2 && style.color.is_some() })
        );
    }
}
