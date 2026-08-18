//! In-process Nushell engine, owned by a background thread.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use nu_cli::{add_cli_context, eval_config_contents, gather_parent_env_vars};
use nu_cmd_extra::add_extra_command_context;
use nu_cmd_lang::create_default_context;
use nu_color_config::StyleComputer;
use nu_command::add_shell_command_context;
use nu_config::{CliOverrides, SystemEnv, resolve_paths};
use nu_engine::eval_block_with_early_return;
use nu_engine::scope::ScopeData;
use nu_parser::parse;
use nu_protocol::debugger::WithoutDebug;
use nu_protocol::engine::{EngineState, Stack, StateWorkingSet};
use nu_protocol::{PipelineData, Signals, Span, Type, Value};
use nu_std::load_standard_library;
use nu_utils::get_ls_colors;

use crate::color::Rgb;
use crate::result::{Cell, CellKind, ResultBody, ResultId, ResultStatus, ResultStore, SortKey};

const DEFAULT_RESULT_CAP: usize = 10;
const NU_VERSION: &str = "0.115.0";

/// Request sent to the engine thread.
pub enum EngineRequest {
    Parse { source: String },
    Eval { id: ResultId, source: String },
    ScopeCommands,
    ScopeVariables,
    Interrupt,
    Shutdown,
}

/// Parse-time feedback for the REPL bar.
#[derive(Debug, Clone)]
pub struct ParseReport {
    pub source: String,
    pub complete: bool,
    pub error: Option<String>,
}

/// Outcome of one Invocation.
#[derive(Debug, Clone)]
pub struct EvalReport {
    pub id: ResultId,
    pub source: String,
    pub status: ResultStatus,
    pub duration: Duration,
    pub exit_code: i64,
    pub body: ResultBody,
}

/// Settings pulled from the live Nushell `config.nu`.
#[derive(Debug, Clone, Default)]
pub struct EngineSnapshot {
    pub nu_version: String,
    pub config_path: String,
    pub env_path: String,
    pub color_config: Vec<(String, String)>,
    pub float_precision: i64,
}

/// Messages the UI consumes.
pub enum EngineEvent {
    Ready(EngineSnapshot),
    Parse(ParseReport),
    Eval(EvalReport),
    ScopeCommands(ResultStore),
    ScopeVariables(ResultStore),
    Failed(String),
}

/// Handle used by the UI thread.
#[derive(Clone)]
pub struct EngineHandle {
    tx: Sender<EngineRequest>,
    interrupt: Arc<AtomicBool>,
}

impl EngineHandle {
    pub fn parse(&self, source: impl Into<String>) {
        let _ = self.tx.send(EngineRequest::Parse {
            source: source.into(),
        });
    }

    pub fn eval(&self, id: ResultId, source: impl Into<String>) {
        self.interrupt.store(false, Ordering::SeqCst);
        let _ = self.tx.send(EngineRequest::Eval {
            id,
            source: source.into(),
        });
    }

    pub fn interrupt(&self) {
        self.interrupt.store(true, Ordering::SeqCst);
        let _ = self.tx.send(EngineRequest::Interrupt);
    }

    pub fn scope_commands(&self) {
        let _ = self.tx.send(EngineRequest::ScopeCommands);
    }

    pub fn scope_variables(&self) {
        let _ = self.tx.send(EngineRequest::ScopeVariables);
    }
}

/// Spawn the engine thread. Returns the UI handle and the event receiver.
pub fn spawn_engine() -> (EngineHandle, Receiver<EngineEvent>) {
    let (req_tx, req_rx) = mpsc::channel();
    let (evt_tx, evt_rx) = mpsc::channel();
    let interrupt = Arc::new(AtomicBool::new(false));
    let handle = EngineHandle {
        tx: req_tx,
        interrupt: interrupt.clone(),
    };

    thread::Builder::new()
        .name("sisko-engine".into())
        .spawn(move || {
            if let Err(err) = run_engine(req_rx, evt_tx.clone(), interrupt) {
                let _ = evt_tx.send(EngineEvent::Failed(format!("{err:#}")));
            }
        })
        .expect("failed to spawn engine thread");

    (handle, evt_rx)
}

fn run_engine(
    req_rx: Receiver<EngineRequest>,
    evt_tx: Sender<EngineEvent>,
    interrupt: Arc<AtomicBool>,
) -> Result<()> {
    let mut engine_state = boot_engine(interrupt, true)?;
    let mut stack = Stack::new();
    let _ = evt_tx.send(EngineEvent::Ready(snapshot(&engine_state)));

    while let Ok(request) = req_rx.recv() {
        match request {
            EngineRequest::Shutdown => break,
            EngineRequest::Interrupt => {
                // Flag is already set by the handle.
            }
            EngineRequest::Parse { source } => {
                let report = parse_source(&engine_state, &source);
                let _ = evt_tx.send(EngineEvent::Parse(report));
            }
            EngineRequest::Eval { id, source } => {
                let report = eval_source(&mut engine_state, &mut stack, id, &source);
                let _ = evt_tx.send(EngineEvent::Eval(report));
            }
            EngineRequest::ScopeCommands => {
                let store = collect_command_store(&engine_state);
                let _ = evt_tx.send(EngineEvent::ScopeCommands(store));
            }
            EngineRequest::ScopeVariables => {
                let store = collect_variable_store(&engine_state, &stack);
                let _ = evt_tx.send(EngineEvent::ScopeVariables(store));
            }
        }
    }

    Ok(())
}

fn boot_engine(interrupt: Arc<AtomicBool>, load_user_config: bool) -> Result<EngineState> {
    let mut engine_state = create_default_context();
    engine_state = add_shell_command_context(engine_state);
    engine_state = add_extra_command_context(engine_state);
    engine_state = add_cli_context(engine_state);
    attach_interrupt(&mut engine_state, interrupt);

    let cwd = std::env::current_dir().context("current directory")?;
    gather_parent_env_vars(&mut engine_state, cwd.as_ref());

    if let Ok((dirs, _)) = resolve_paths(&SystemEnv, &CliOverrides::default()) {
        engine_state.config_dirs = dirs;
        engine_state.generate_nu_constant();
        load_standard_library(&mut engine_state)
            .map_err(|err| anyhow::anyhow!("load std library: {err}"))?;
        if load_user_config {
            let mut stack = Stack::new();
            seed_lib_dirs(&mut engine_state)?;
            let env_path = engine_state.config_dirs.env_file.to_path_buf();
            let config_path = engine_state.config_dirs.config_file.to_path_buf();
            eval_config_contents(env_path, &mut engine_state, &mut stack, false);
            eval_config_contents(config_path, &mut engine_state, &mut stack, false);
            engine_state.generate_nu_constant();
        }
    }

    Ok(engine_state)
}

fn snapshot(engine_state: &EngineState) -> EngineSnapshot {
    let config = engine_state.get_config();
    let mut color_config = Vec::new();
    for (key, value) in &config.color_config {
        if let Some(name) = color_name(value) {
            color_config.push((key.clone(), name));
        }
    }
    EngineSnapshot {
        nu_version: NU_VERSION.into(),
        config_path: engine_state
            .config_dirs
            .config_file
            .as_path()
            .display()
            .to_string(),
        env_path: engine_state
            .config_dirs
            .env_file
            .as_path()
            .display()
            .to_string(),
        color_config,
        float_precision: config.float_precision,
    }
}

fn color_name(value: &Value) -> Option<String> {
    match value {
        Value::String { val, .. } => Some(val.clone()),
        Value::Record { val, .. } => val.get("fg").and_then(|fg| match fg {
            Value::String { val, .. } => Some(val.clone()),
            _ => None,
        }),
        _ => None,
    }
}

/// Official `nu` publishes `$NU_LIB_DIRS` and `$env.NU_LIB_DIRS` before `env.nu`
/// / `config.nu` so parse-time `source defs.nu` can resolve through the scripts dir.
fn seed_lib_dirs(engine_state: &mut EngineState) -> Result<()> {
    let mut paths = Vec::new();
    collect_existing_lib_dirs(engine_state, &mut paths);

    let dirs = &engine_state.config_dirs;
    for path in [
        dirs.config_home.join("scripts"),
        dirs.config_home.clone(),
        dirs.data_home.join("completions"),
    ] {
        push_unique_path(&mut paths, path.display().to_string());
    }

    install_lib_dirs(engine_state, &paths)
}

fn collect_existing_lib_dirs(engine_state: &EngineState, paths: &mut Vec<String>) {
    let Some(val) = engine_state.get_env_var("NU_LIB_DIRS") else {
        return;
    };
    match val {
        Value::List { vals, .. } => {
            for value in vals {
                if let Ok(s) = value.as_str() {
                    push_unique_path(paths, s.to_string());
                }
            }
        }
        Value::String { val, .. } => {
            let seps: &[char] = if cfg!(windows) { &[';'] } else { &[':'] };
            for part in val.split(seps) {
                let part = part.trim();
                if !part.is_empty() {
                    push_unique_path(paths, part.to_string());
                }
            }
        }
        _ => {}
    }
}

fn push_unique_path(paths: &mut Vec<String>, path: String) {
    if !path.is_empty() && !paths.iter().any(|existing| existing == &path) {
        paths.push(path);
    }
}

fn install_lib_dirs(engine_state: &mut EngineState, paths: &[String]) -> Result<()> {
    let span = Span::unknown();
    let values: Vec<Value> = paths
        .iter()
        .map(|path| Value::string(path.clone(), span))
        .collect();
    let list = Value::list(values, span);

    engine_state.add_env_var("NU_LIB_DIRS".into(), list.clone());

    let mut working_set = StateWorkingSet::new(engine_state);
    let var_id = working_set.add_variable(
        b"$NU_LIB_DIRS".into(),
        span,
        Type::List(Box::new(Type::String)),
        false,
    );
    working_set.set_variable_const_val(var_id, list);
    engine_state
        .merge_delta(working_set.render())
        .context("merge NU_LIB_DIRS")?;
    Ok(())
}

fn attach_interrupt(engine_state: &mut EngineState, interrupt: Arc<AtomicBool>) {
    engine_state.set_signals(Signals::new(interrupt));
}

fn parse_source(engine_state: &EngineState, source: &str) -> ParseReport {
    let mut working_set = StateWorkingSet::new(engine_state);
    let _block = parse(&mut working_set, None, source.as_bytes(), false);
    let complete = !is_incomplete(&working_set, source);
    let error = first_parse_error(&working_set);
    ParseReport {
        source: source.to_string(),
        complete,
        error,
    }
}

fn first_parse_error(working_set: &StateWorkingSet<'_>) -> Option<String> {
    working_set.parse_errors.first().map(|err| err.to_string())
}

/// Cheap completeness check when the last parse report is stale or missing.
pub fn source_looks_complete(source: &str) -> bool {
    let trimmed = source.trim();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed.ends_with('|') || trimmed.ends_with("out>") {
        return false;
    }
    let opens = trimmed
        .chars()
        .filter(|c| matches!(c, '{' | '(' | '['))
        .count();
    let closes = trimmed
        .chars()
        .filter(|c| matches!(c, '}' | ')' | ']'))
        .count();
    opens <= closes
}

fn is_incomplete(working_set: &StateWorkingSet<'_>, source: &str) -> bool {
    if !source_looks_complete(source) {
        return true;
    }
    working_set.parse_errors.iter().any(|err| {
        let text = err.to_string().to_lowercase();
        text.contains("unexpected end")
            || text.contains("unexpected eof")
            || text.contains("incomplete")
            || text.contains("unclosed")
    })
}

fn collect_command_store(engine_state: &EngineState) -> ResultStore {
    let mut rows = Vec::new();
    for (name, decl_id) in engine_state.get_decls_sorted(false) {
        let decl = engine_state.get_decl(decl_id);
        if decl.is_alias() {
            continue;
        }
        rows.push(vec![
            Cell::text(String::from_utf8_lossy(&name)),
            Cell::text(decl.signature().category.to_string()),
            Cell::text(decl.command_type().to_string()),
            Cell::text(decl.description()),
        ]);
    }
    ResultStore {
        columns: vec![
            "name".into(),
            "category".into(),
            "type".into(),
            "description".into(),
        ],
        rows,
    }
}

fn collect_variable_store(engine_state: &EngineState, stack: &Stack) -> ResultStore {
    let styles = StyleComputer::from_config(engine_state, stack);
    let config = engine_state.get_config();
    let mut scope = ScopeData::new(engine_state, stack);
    scope.populate_vars();
    let vals = scope.collect_vars(Span::unknown());
    let mut rows: Vec<Vec<Cell>> = vals
        .iter()
        .filter_map(|value| variable_row(value, engine_state, stack, &styles, config.as_ref()))
        .collect();
    if rows.is_empty() {
        return env_fallback_store(engine_state, stack, &styles, None, None);
    }
    rows.sort_by(|a, b| a[0].text.to_lowercase().cmp(&b[0].text.to_lowercase()));
    ResultStore {
        columns: vec![
            "name".into(),
            "type".into(),
            "value".into(),
            "is_const".into(),
        ],
        rows,
    }
}

fn variable_row(
    value: &Value,
    engine_state: &EngineState,
    stack: &Stack,
    styles: &StyleComputer<'_>,
    config: &nu_protocol::Config,
) -> Option<Vec<Cell>> {
    let Value::Record { val, .. } = value else {
        return None;
    };
    let name = val.get("name")?.as_str().ok()?.to_string();
    let ty = val
        .get("type")
        .and_then(|v| v.as_str().ok())
        .unwrap_or("any")
        .to_string();
    let is_const = val
        .get("is_const")
        .and_then(|v| v.as_bool().ok())
        .unwrap_or(false);
    let raw = val.get("value");
    let value_cell = match raw {
        Some(inner) => shallow_value_cell(inner, engine_state, stack, styles, config),
        None => Cell::text("—"),
    };
    Some(vec![
        Cell::text(name),
        Cell::text(ty),
        value_cell,
        Cell::text(if is_const { "true" } else { "false" }),
    ])
}

/// One level of structure for the Variables dock — never walk `$env.config` deeply.
fn shallow_value_cell(
    value: &Value,
    engine_state: &EngineState,
    stack: &Stack,
    styles: &StyleComputer<'_>,
    config: &nu_protocol::Config,
) -> Cell {
    match value {
        Value::Record { val, .. } => {
            let label = format!("{} fields", val.len());
            let rows = val
                .iter()
                .map(|(key, inner)| vec![Cell::text(key), shallow_leaf(inner, config)])
                .collect();
            Cell {
                text: label.clone(),
                kind: CellKind::Other,
                sort: SortKey::Text(label.to_lowercase()),
                color: None,
                bold: false,
                nested: Some(Box::new(ResultStore {
                    columns: vec!["field".into(), "value".into()],
                    rows,
                })),
            }
        }
        Value::List { vals, .. } => {
            if let Some(text) = compact_value_text(value, config) {
                return Cell::text(text);
            }
            let label = format!("{} items", vals.len());
            let rows = vals
                .iter()
                .take(40)
                .map(|inner| vec![shallow_leaf(inner, config)])
                .collect();
            Cell {
                text: label.clone(),
                kind: CellKind::Other,
                sort: SortKey::Text(label.to_lowercase()),
                color: None,
                bold: false,
                nested: Some(Box::new(ResultStore {
                    columns: vec!["value".into()],
                    rows,
                })),
            }
        }
        other => value_cell(
            "value",
            other,
            engine_state,
            styles,
            ls_colors_from_stack(engine_state, stack).as_ref(),
            None,
            0,
        ),
    }
}

fn shallow_leaf(value: &Value, config: &nu_protocol::Config) -> Cell {
    match value {
        Value::Record { val, .. } => {
            let label = format!("{} fields", val.len());
            let rows = val
                .iter()
                .map(|(key, inner)| {
                    vec![
                        Cell::text(key),
                        Cell::text(inner.to_abbreviated_string(config)),
                    ]
                })
                .collect();
            Cell {
                text: label.clone(),
                kind: CellKind::Other,
                sort: SortKey::Text(label.to_lowercase()),
                color: None,
                bold: false,
                nested: Some(Box::new(ResultStore {
                    columns: vec!["field".into(), "value".into()],
                    rows,
                })),
            }
        }
        Value::List { vals, .. } => {
            if let Some(text) = compact_value_text(value, config) {
                return Cell::text(text);
            }
            Cell::text(format!("{} items", vals.len()))
        }
        other => Cell::text(other.to_abbreviated_string(config)),
    }
}

fn env_fallback_store(
    engine_state: &EngineState,
    stack: &Stack,
    styles: &StyleComputer<'_>,
    ls_colors: Option<&lscolors::LsColors>,
    cwd: Option<&std::path::Path>,
) -> ResultStore {
    let env_value = env_record(engine_state, stack);
    let env_cell = value_cell("value", &env_value, engine_state, styles, ls_colors, cwd, 2);
    ResultStore {
        columns: vec![
            "name".into(),
            "type".into(),
            "value".into(),
            "is_const".into(),
        ],
        rows: vec![vec![
            Cell::text("$env"),
            Cell::text("record"),
            env_cell,
            Cell::text("false"),
        ]],
    }
}

fn env_record(engine_state: &EngineState, stack: &Stack) -> Value {
    let span = Span::unknown();
    let mut record = nu_protocol::Record::new();
    for (name, value) in stack.get_env_vars(engine_state) {
        record.insert(name, value);
    }
    Value::record(record, span)
}

fn eval_source(
    engine_state: &mut EngineState,
    stack: &mut Stack,
    id: ResultId,
    source: &str,
) -> EvalReport {
    let started = Instant::now();
    let mut working_set = StateWorkingSet::new(engine_state);
    let block = parse(&mut working_set, None, source.as_bytes(), false);

    if let Some(err) = working_set.parse_errors.first() {
        return EvalReport {
            id,
            source: source.to_string(),
            status: ResultStatus::Failed,
            duration: started.elapsed(),
            exit_code: 1,
            body: ResultBody::Diagnostic(err.to_string()),
        };
    }

    if let Err(err) = engine_state.merge_delta(working_set.render()) {
        return EvalReport {
            id,
            source: source.to_string(),
            status: ResultStatus::Failed,
            duration: started.elapsed(),
            exit_code: 1,
            body: ResultBody::Diagnostic(err.to_string()),
        };
    }

    match eval_block_with_early_return::<WithoutDebug>(
        engine_state,
        stack,
        &block,
        PipelineData::empty(),
    ) {
        Ok(exec) => {
            let body = pipeline_to_body(exec.body, engine_state, stack);
            EvalReport {
                id,
                source: source.to_string(),
                status: ResultStatus::Succeeded,
                duration: started.elapsed(),
                exit_code: 0,
                body,
            }
        }
        Err(err) => {
            let interrupted = err.to_string().to_lowercase().contains("interrupt");
            EvalReport {
                id,
                source: source.to_string(),
                status: if interrupted {
                    ResultStatus::Interrupted
                } else {
                    ResultStatus::Failed
                },
                duration: started.elapsed(),
                exit_code: 1,
                body: ResultBody::Diagnostic(err.to_string()),
            }
        }
    }
}

fn pipeline_to_body(data: PipelineData, engine_state: &EngineState, stack: &Stack) -> ResultBody {
    let value = match data.into_value(Span::unknown()) {
        Ok(value) => value,
        Err(err) => return ResultBody::Diagnostic(err.to_string()),
    };
    let styles = StyleComputer::from_config(engine_state, stack);
    let ls_colors = ls_colors_from_stack(engine_state, stack);
    let cwd = std::env::current_dir().ok();
    value_to_body(
        value,
        engine_state,
        &styles,
        ls_colors.as_ref(),
        cwd.as_deref(),
        8,
    )
}

fn ls_colors_from_stack(engine_state: &EngineState, stack: &Stack) -> Option<lscolors::LsColors> {
    if !engine_state.get_config().ls.use_ls_colors {
        return None;
    }
    let raw = stack
        .get_env_var(engine_state, "LS_COLORS")
        .and_then(|v| v.as_str().ok().map(ToString::to_string));
    Some(get_ls_colors(raw))
}

fn value_to_body(
    value: Value,
    engine_state: &EngineState,
    styles: &StyleComputer<'_>,
    ls_colors: Option<&lscolors::LsColors>,
    cwd: Option<&std::path::Path>,
    nest_depth: usize,
) -> ResultBody {
    match value {
        Value::Nothing { .. } => ResultBody::Empty,
        Value::Binary { val, .. } => ResultBody::Binary {
            bytes: val.len(),
            summary: format!("{} bytes", val.len()),
        },
        Value::String { val, .. } => {
            if val.contains('\n') {
                ResultBody::Text(val)
            } else {
                ResultBody::Scalar(val)
            }
        }
        Value::List { vals, .. } => list_to_body(
            vals.to_vec(),
            engine_state,
            styles,
            ls_colors,
            cwd,
            nest_depth,
        ),
        Value::Record { val, .. } => {
            if should_pair_record(&val) {
                return nested_store(
                    &Value::record((*val).clone(), Span::unknown()),
                    engine_state,
                    styles,
                    ls_colors,
                    cwd,
                    nest_depth.max(2),
                )
                .map(ResultBody::Table)
                .unwrap_or(ResultBody::Empty);
            }
            let columns: Vec<String> = val.columns().map(ToString::to_string).collect();
            let row: Vec<Cell> = val
                .iter()
                .map(|(col, cell)| {
                    value_cell(col, cell, engine_state, styles, ls_colors, cwd, nest_depth)
                })
                .collect();
            ResultBody::Table(ResultStore {
                columns,
                rows: vec![row],
            })
        }
        other => ResultBody::Scalar(
            value_cell(
                "value",
                &other,
                engine_state,
                styles,
                ls_colors,
                cwd,
                nest_depth,
            )
            .text,
        ),
    }
}

fn should_pair_record(record: &nu_protocol::Record) -> bool {
    record.len() > 6
        || record
            .iter()
            .any(|(_, value)| matches!(value, Value::Record { .. } | Value::List { .. }))
}

fn is_scalar_leaf(value: &Value) -> bool {
    matches!(
        value,
        Value::Nothing { .. }
            | Value::Bool { .. }
            | Value::Int { .. }
            | Value::Float { .. }
            | Value::String { .. }
            | Value::Filesize { .. }
            | Value::Duration { .. }
            | Value::Date { .. }
            | Value::Binary { .. }
    )
}

fn scalar_text(value: &Value, config: &nu_protocol::Config) -> String {
    match value {
        Value::Nothing { .. } => "null".into(),
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
        Value::Binary { val, .. } => format!(
            "0x[{}]",
            val.iter().map(|b| format!("{b:02x}")).collect::<String>()
        ),
        other => other.to_abbreviated_string(config),
    }
}

/// Compact `[1, 2]` / `[[1, 2], [3, 4]]` the way `table -e` prints leaf lists.
fn compact_value_text(value: &Value, config: &nu_protocol::Config) -> Option<String> {
    compact_value_text_depth(value, config, 3)
}

fn compact_value_text_depth(
    value: &Value,
    config: &nu_protocol::Config,
    depth: usize,
) -> Option<String> {
    if depth == 0 {
        return None;
    }
    let Value::List { vals, .. } = value else {
        return None;
    };
    if vals.len() > 24 {
        return None;
    }
    if vals.is_empty() {
        return Some("[]".into());
    }
    let mut parts = Vec::with_capacity(vals.len());
    for item in vals.iter() {
        if is_scalar_leaf(item) {
            parts.push(scalar_text(item, config));
        } else if let Some(inner) = compact_value_text_depth(item, config, depth - 1) {
            parts.push(inner);
        } else {
            return None;
        }
    }
    Some(format!("[{}]", parts.join(", ")))
}

fn list_to_body(
    vals: Vec<Value>,
    engine_state: &EngineState,
    styles: &StyleComputer<'_>,
    ls_colors: Option<&lscolors::LsColors>,
    cwd: Option<&std::path::Path>,
    nest_depth: usize,
) -> ResultBody {
    if vals.is_empty() {
        return ResultBody::Empty;
    }
    if vals.iter().all(|v| matches!(v, Value::Record { .. })) {
        let mut columns = Vec::new();
        for val in &vals {
            if let Value::Record { val, .. } = val {
                for col in val.columns() {
                    if !columns.iter().any(|c| c == col) {
                        columns.push(col.to_string());
                    }
                }
            }
        }
        let rows = vals
            .iter()
            .map(|val| {
                columns
                    .iter()
                    .map(|col| match val {
                        Value::Record { val, .. } => val
                            .get(col)
                            .map(|cell| {
                                value_cell(
                                    col,
                                    cell,
                                    engine_state,
                                    styles,
                                    ls_colors,
                                    cwd,
                                    nest_depth,
                                )
                            })
                            .unwrap_or_default(),
                        _ => Cell::default(),
                    })
                    .collect()
            })
            .collect();
        return ResultBody::Table(ResultStore { columns, rows });
    }

    ResultBody::Table(ResultStore {
        columns: vec!["value".into()],
        rows: vals
            .into_iter()
            .map(|v| {
                vec![value_cell(
                    "value",
                    &v,
                    engine_state,
                    styles,
                    ls_colors,
                    cwd,
                    nest_depth,
                )]
            })
            .collect(),
    })
}

fn value_cell(
    column: &str,
    value: &Value,
    engine_state: &EngineState,
    styles: &StyleComputer<'_>,
    ls_colors: Option<&lscolors::LsColors>,
    cwd: Option<&std::path::Path>,
    nest_depth: usize,
) -> Cell {
    let config = engine_state.get_config();
    let mut nested = None;
    let (kind, sort, text) = match value {
        Value::String { val, .. } => (
            CellKind::Text,
            SortKey::Text(val.to_lowercase()),
            val.clone(),
        ),
        Value::Int { val, .. } => (CellKind::Int, SortKey::Int(*val), val.to_string()),
        Value::Float { val, .. } => (
            CellKind::Float,
            SortKey::Float(*val),
            format!(
                "{val:.prec$}",
                prec = config.float_precision.max(0) as usize
            ),
        ),
        Value::Bool { val, .. } => (
            CellKind::Bool,
            SortKey::Int(i64::from(*val)),
            val.to_string(),
        ),
        Value::Nothing { .. } => (CellKind::Empty, SortKey::Empty, String::new()),
        Value::Filesize { val, .. } => (
            CellKind::Filesize,
            SortKey::Int(i64::from(*val)),
            config.filesize.format(*val).to_string(),
        ),
        Value::Duration { val, .. } => (
            CellKind::Duration,
            SortKey::Int(*val),
            value.to_abbreviated_string(config),
        ),
        Value::Date { val, .. } => (
            CellKind::Date,
            SortKey::Int(val.timestamp_nanos_opt().unwrap_or(0)),
            value.to_abbreviated_string(config),
        ),
        Value::Closure { .. } => (
            CellKind::Other,
            SortKey::Text("<closure>".into()),
            "<closure>".into(),
        ),
        Value::Record { .. } | Value::List { .. } => {
            if let Some(text) = compact_value_text(value, config) {
                (CellKind::Other, SortKey::Text(text.to_lowercase()), text)
            } else {
                let store = nested_store(value, engine_state, styles, ls_colors, cwd, nest_depth);
                let label = nested_label(value, store.as_ref());
                nested = store.map(Box::new);
                (CellKind::Other, SortKey::Text(label.to_lowercase()), label)
            }
        }
        other => (
            CellKind::Other,
            SortKey::Text(other.to_abbreviated_string(config).to_lowercase()),
            other.to_expanded_string(", ", config),
        ),
    };

    let mut rgb = style_for_scalar(styles, kind, value);
    if column == "name" {
        if let Some(path_style) = ls_color_for_name(value, ls_colors, cwd) {
            rgb = Some(path_style);
        }
    }

    if nested.is_none() && text.contains('\n') {
        nested = Some(Box::new(ResultStore::text_block(text.clone())));
    }

    Cell {
        text,
        kind,
        sort,
        color: rgb.map(|c| (c.r, c.g, c.b)),
        bold: rgb.map(|c| c.bold).unwrap_or(false),
        nested,
    }
}

fn nested_label(value: &Value, store: Option<&ResultStore>) -> String {
    if let Some(store) = store {
        let rows = store.row_count();
        let cols = store.columns.len();
        return match rows {
            1 if cols <= 4 => format!("1 row · {cols} fields"),
            1 => "1 row".into(),
            n => format!("{n} rows"),
        };
    }
    match value {
        Value::List { vals, .. } if vals.iter().all(|v| matches!(v, Value::Record { .. })) => {
            format!("{} rows", vals.len())
        }
        Value::List { vals, .. } => format!("{} items", vals.len()),
        Value::Record { val, .. } => format!("{} fields", val.len()),
        _ => "table".into(),
    }
}

fn nested_store(
    value: &Value,
    engine_state: &EngineState,
    styles: &StyleComputer<'_>,
    ls_colors: Option<&lscolors::LsColors>,
    cwd: Option<&std::path::Path>,
    depth: usize,
) -> Option<ResultStore> {
    if depth == 0 {
        return None;
    }
    match value {
        Value::Record { val, .. } => {
            if let Some(store) = flatten_signature_record(val, engine_state, styles, ls_colors, cwd)
            {
                return Some(store);
            }
            let columns = vec!["field".into(), "value".into()];
            let rows = val
                .iter()
                .map(|(key, inner)| {
                    vec![
                        Cell::text(key),
                        value_cell(
                            "value",
                            inner,
                            engine_state,
                            styles,
                            ls_colors,
                            cwd,
                            depth.saturating_sub(1),
                        ),
                    ]
                })
                .collect();
            Some(ResultStore { columns, rows })
        }
        Value::List { vals, .. } if vals.iter().all(|v| matches!(v, Value::Record { .. })) => {
            match list_to_body(
                vals.to_vec(),
                engine_state,
                styles,
                ls_colors,
                cwd,
                depth.saturating_sub(1).max(1),
            ) {
                ResultBody::Table(store) => Some(store),
                _ => None,
            }
        }
        Value::List { vals, .. } => {
            let columns = vec!["value".into()];
            let rows = vals
                .iter()
                .map(|v| {
                    vec![value_cell(
                        "value",
                        v,
                        engine_state,
                        styles,
                        ls_colors,
                        cwd,
                        depth.saturating_sub(1),
                    )]
                })
                .collect();
            Some(ResultStore { columns, rows })
        }
        _ => None,
    }
}

/// `scope commands` stores signatures as `{ "": [param records] }`. Unwrap that
/// so Inspect opens a real parameter table instead of a one-field wrapper.
fn flatten_signature_record(
    record: &nu_protocol::Record,
    engine_state: &EngineState,
    styles: &StyleComputer<'_>,
    ls_colors: Option<&lscolors::LsColors>,
    cwd: Option<&std::path::Path>,
) -> Option<ResultStore> {
    if record.len() != 1 {
        return None;
    }
    let (_, inner) = record.iter().next()?;
    match inner {
        Value::List { vals, .. }
            if !vals.is_empty() && vals.iter().all(|v| matches!(v, Value::Record { .. })) =>
        {
            match list_to_body(vals.to_vec(), engine_state, styles, ls_colors, cwd, 2) {
                ResultBody::Table(store) => Some(store),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Only style scalars. `color_config.string` is a closure that runs `=~` on `$in`;
/// applying it to lists/tables (signatures, examples) floods the log with errors.
fn style_for_scalar(styles: &StyleComputer<'_>, kind: CellKind, value: &Value) -> Option<Rgb> {
    if !matches!(
        value,
        Value::String { .. }
            | Value::Int { .. }
            | Value::Float { .. }
            | Value::Bool { .. }
            | Value::Filesize { .. }
            | Value::Duration { .. }
            | Value::Date { .. }
            | Value::Nothing { .. }
    ) {
        return None;
    }
    Rgb::from_style(styles.compute(style_key(kind), value))
}

fn style_key(kind: CellKind) -> &'static str {
    match kind {
        CellKind::Text => "string",
        CellKind::Int => "int",
        CellKind::Float => "float",
        CellKind::Bool => "bool",
        CellKind::Filesize => "filesize",
        CellKind::Duration => "duration",
        CellKind::Date => "datetime",
        CellKind::Empty => "empty",
        CellKind::Other => "string",
    }
}

fn ls_color_for_name(
    value: &Value,
    ls_colors: Option<&lscolors::LsColors>,
    cwd: Option<&std::path::Path>,
) -> Option<Rgb> {
    let ls_colors = ls_colors?;
    let Value::String { val, .. } = value else {
        return None;
    };
    let path = match cwd {
        Some(cwd) => cwd.join(val),
        None => std::path::PathBuf::from(val),
    };
    let meta = std::fs::symlink_metadata(&path).ok();
    let style = ls_colors.style_for_path_with_metadata(val, meta.as_ref())?;
    Rgb::from_style(style.to_nu_ansi_term_style())
}

/// Settings the Session uses for Result retention.
pub fn result_cap() -> usize {
    DEFAULT_RESULT_CAP
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_source_is_incomplete() {
        let engine = create_default_context();
        let report = parse_source(&engine, "");
        assert!(!report.complete);
    }

    #[test]
    fn format_number_is_registered() {
        let engine = boot_engine(Arc::new(AtomicBool::new(false)), false).expect("boot");
        let report = parse_source(&engine, "42 | format number");
        assert!(
            report.error.is_none(),
            "format number should parse like official nu, got {:?}",
            report.error
        );
        assert!(report.complete);
    }

    #[test]
    fn ls_is_complete() {
        let engine = create_default_context();
        let report = parse_source(&engine, "ls");
        assert!(report.complete, "ls parse error: {:?}", report.error);
    }

    #[test]
    fn trailing_pipe_is_incomplete() {
        assert!(!source_looks_complete("ls |"));
        assert!(source_looks_complete("ls"));
        assert!(source_looks_complete("ls\n"));
    }

    #[test]
    fn eval_math_pipeline() {
        let mut engine = boot_engine(Arc::new(AtomicBool::new(false)), false).expect("boot engine");
        let mut stack = Stack::new();
        let report = eval_source(&mut engine, &mut stack, ResultId(1), "1 + 1");
        assert_eq!(report.status, ResultStatus::Succeeded);
        match report.body {
            ResultBody::Scalar(value) => assert_eq!(value, "2"),
            other => panic!("expected scalar 2, got {other:?}"),
        }
    }

    #[test]
    fn user_config_can_source_defs_nu() {
        let engine = boot_engine(Arc::new(AtomicBool::new(false)), true).expect("boot with config");
        let defs = engine
            .config_dirs
            .config_home
            .join("scripts")
            .join("defs.nu");
        if !defs.exists() {
            return;
        }
        let report = parse_source(&engine, "source defs.nu");
        let missing = report.error.as_deref().is_some_and(|err| {
            let err = err.to_lowercase();
            err.contains("not found") || err.contains("file not found")
        });
        assert!(
            !missing,
            "expected source defs.nu to resolve via $NU_LIB_DIRS, got {:?}",
            report.error
        );
    }

    #[test]
    fn source_resolves_via_seeded_lib_dirs() {
        let mut engine = boot_engine(Arc::new(AtomicBool::new(false)), false).expect("boot engine");
        let dir = std::env::temp_dir().join(format!("sisko-lib-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp lib dir");
        std::fs::write(
            dir.join("defs.nu"),
            "export-env { $env.SISKO_DEFS = 'ok' }\n",
        )
        .expect("write defs.nu");
        install_lib_dirs(&mut engine, &[dir.display().to_string()]).expect("install lib dirs");

        let report = parse_source(&engine, "source defs.nu");
        assert!(
            report.error.is_none(),
            "source defs.nu should resolve via $NU_LIB_DIRS, got {:?}",
            report.error
        );
        assert!(report.complete);
    }

    #[test]
    fn nested_signature_record_flattens_to_rows() {
        let engine = create_default_context();
        let stack = Stack::new();
        let styles = StyleComputer::from_config(&engine, &stack);
        let params = Value::test_list(vec![
            Value::test_record(nu_protocol::record!(
                "parameter" => Value::test_string("path"),
                "type" => Value::test_string("string"),
            )),
            Value::test_record(nu_protocol::record!(
                "parameter" => Value::test_string("all"),
                "type" => Value::test_string("switch"),
            )),
        ]);
        let signatures = Value::test_record(nu_protocol::record!("" => params));
        let store =
            nested_store(&signatures, &engine, &styles, None, None, 3).expect("nested store");
        assert_eq!(
            store.columns,
            vec!["parameter".to_string(), "type".to_string()]
        );
        assert_eq!(store.row_count(), 2);
        assert_eq!(store.rows[0][0].text, "path");
    }

    #[test]
    fn scope_variables_query_returns_rows() {
        let engine = boot_engine(Arc::new(AtomicBool::new(false)), false).expect("boot");
        let stack = Stack::new();
        let commands = collect_command_store(&engine);
        assert!(
            commands.row_count() > 10,
            "expected a command catalog, got {} rows",
            commands.row_count()
        );
        assert!(commands.rows.iter().any(|row| row[0].text == "ls"));
        let store = collect_variable_store(&engine, &stack);
        assert!(
            store.columns.first().map(String::as_str) != Some("error"),
            "scope variables failed: {:?}",
            store
                .rows
                .first()
                .map(|row| { row.iter().map(|c| c.text.clone()).collect::<Vec<_>>() })
        );
        assert!(store.row_count() > 0, "expected at least one variable");
        assert!(store.columns.iter().any(|c| c == "name"));
    }

    #[test]
    fn compact_nested_int_lists() {
        let config = nu_protocol::Config::default();
        let value = Value::test_list(vec![
            Value::test_list(vec![Value::test_int(1), Value::test_int(2)]),
            Value::test_list(vec![Value::test_int(3), Value::test_int(4)]),
        ]);
        assert_eq!(
            compact_value_text(&value, &config).as_deref(),
            Some("[[1, 2], [3, 4]]")
        );
    }

    #[test]
    fn large_or_nested_record_becomes_field_value_table() {
        let engine = create_default_context();
        let stack = Stack::new();
        let styles = StyleComputer::from_config(&engine, &stack);
        let record = nu_protocol::record!(
            "ls" => Value::test_record(nu_protocol::record!(
                "use_ls_colors" => Value::test_bool(true),
            )),
            "table" => Value::test_record(nu_protocol::record!(
                "mode" => Value::test_string("compact"),
            )),
        );
        let body = value_to_body(Value::test_record(record), &engine, &styles, None, None, 8);
        match body {
            ResultBody::Table(store) => {
                assert_eq!(
                    store.columns,
                    vec!["field".to_string(), "value".to_string()]
                );
                assert_eq!(store.row_count(), 2);
            }
            other => panic!("expected field/value table, got {other:?}"),
        }
    }

    #[test]
    fn multiline_string_becomes_expandable_text_block() {
        let engine = create_default_context();
        let stack = Stack::new();
        let styles = StyleComputer::from_config(&engine, &stack);
        let cell = value_cell(
            "extra_description",
            &Value::test_string("one\ntwo\nthree"),
            &engine,
            &styles,
            None,
            None,
            8,
        );
        assert!(cell.text.contains("two"));
        assert_eq!(cell.expand_label(), "3 lines");
        assert!(
            cell.nested
                .as_deref()
                .is_some_and(ResultStore::is_text_block)
        );
    }

    #[test]
    fn deep_menus_and_explore_keep_expandable_stores() {
        let engine = create_default_context();
        let stack = Stack::new();
        let styles = StyleComputer::from_config(&engine, &stack);
        let items: Vec<Value> = (0..13)
            .map(|i| {
                Value::test_record(nu_protocol::record!(
                    "value" => Value::test_string(format!("item-{i}")),
                ))
            })
            .collect();
        let record = nu_protocol::record!(
            "explore" => Value::test_record(nu_protocol::record!(
                "table" => Value::test_record(nu_protocol::record!(
                    "selected_cell" => Value::test_record(nu_protocol::record!(
                        "bg" => Value::test_string("blue"),
                        "fg" => Value::test_string("white"),
                    )),
                )),
            )),
            "menus" => Value::test_list(vec![Value::test_record(nu_protocol::record!(
                "name" => Value::test_string("ide_completion"),
                "marker" => Value::test_string("| "),
                "items" => Value::test_list(items),
            ))]),
        );
        let body = value_to_body(Value::test_record(record), &engine, &styles, None, None, 8);
        let ResultBody::Table(root) = body else {
            panic!("expected table");
        };
        let explore = root
            .rows
            .iter()
            .find(|row| row[0].text == "explore")
            .and_then(|row| row[1].nested.as_deref())
            .expect("explore nested");
        let table = explore
            .rows
            .iter()
            .find(|row| row[0].text == "table")
            .and_then(|row| row[1].nested.as_deref())
            .expect("table nested");
        let selected = table
            .rows
            .iter()
            .find(|row| row[0].text == "selected_cell")
            .and_then(|row| row[1].nested.as_deref())
            .expect("selected_cell nested");
        assert!(selected.row_count() >= 2);

        let menus = root
            .rows
            .iter()
            .find(|row| row[0].text == "menus")
            .and_then(|row| row[1].nested.as_deref())
            .expect("menus nested");
        let items = menus.rows[0]
            .iter()
            .find(|cell| cell.text.contains("13"))
            .and_then(|cell| cell.nested.as_deref())
            .expect("ide_completion items nested");
        assert_eq!(items.row_count(), 13);
    }
}
