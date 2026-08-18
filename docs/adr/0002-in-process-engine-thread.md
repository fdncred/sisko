# In-process engine on a dedicated thread

Nushell runs in-process (`EngineState` + `Stack`) so parse, complete, help, `scope commands`, `$ans`, and `PipelineData` stay native. Eval and parse run on a dedicated engine thread so the GPUI UI thread never blocks. A `nu` subprocess would force serialization, break live session features, and cannot show million-row tables as Values.
