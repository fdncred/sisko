# Columnar Result store with a cap and optional spill

Each table Result owns a compact columnar store, not `Vec<Value>`. The UI keeps at most N Results (default 10). On overflow, the oldest body spills to disk if that setting is on, otherwise it is dropped. Sort/filter are view-only on the store. `$ans.last` is a separate, size-capped Nu Value from the official REPL.
