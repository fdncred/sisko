# Sisko

A desktop Nushell workbench: a full Nushell session with a professional table UI, not a terminal emulator and not a Nu-scriptable widget toolkit.

## Language

**Sisko**:
A desktop Nushell workbench: a full Nushell session with a professional table UI, not a terminal emulator and not a Nu-scriptable widget toolkit.
_Avoid_: terminal, IDE, notebook

**Session**:
One window’s persistent Nushell engine (`config.nu`, env, plugins, definitions) plus the Results produced in that window.
_Avoid_: process, REPL

**REPL bar**:
The bottom editor where the user constructs and submits pipelines.
_Avoid_: terminal, docking bar, console

**Invocation**:
One submitted pipeline: the source text committed from the REPL bar.
_Avoid_: command line, cell, query

**Result**:
The UI object created by one Invocation. Header (source, status, duration) plus a typed body.
_Avoid_: segment, tabular window, output window, cell

**Result body**:
The typed view inside a Result: table, single-value, text, diagnostic, binary summary, or empty. `print`/warnings are a log strip on that Result, not a second Result.

**$ans**:
Nushell’s built-in last-Invocation record (`last`, `exit_code`, `duration`), including `$env.config.last_result_size`.
_Avoid_: `$last`, `$_`, `$r1`

**Command catalog**:
The Help-dock table of live EngineState declarations, in the spirit of `scope commands`.

**Result store**:
The compact columnar (or spilled-to-disk) backing data for a table Result. Not `Vec<Value>`, not `$ans.last`.
