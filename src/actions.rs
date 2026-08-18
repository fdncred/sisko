use gpui::actions;

actions!(
    sisko,
    [
        SubmitPipeline,
        StopPipeline,
        ToggleHelp,
        ToggleHistory,
        ToggleVariables,
        NewWindow,
        ThemeLight,
        ThemeDark,
        ThemeToggle,
        OpenSettings,
        AutoResizeColumns,
        Quit,
    ]
);
