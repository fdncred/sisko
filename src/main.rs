use gpui::{
    App, AppContext, Bounds, KeyBinding, Menu, MenuItem, WindowBounds, WindowKind, point, px, size,
};
use gpui_component::{ActiveTheme, Root, TitleBar};
use gpui_component::{Theme, ThemeMode};
use sisko::actions::{AutoResizeColumns, OpenSettings, Quit, ThemeDark, ThemeLight, ThemeToggle};
use sisko::engine::spawn_engine;
use sisko::workspace::Workspace;

fn main() {
    let app = gpui_platform::application().with_assets(gpui_component_assets::Assets);

    app.run(move |cx| {
        gpui_component::init(cx);
        init_app_chrome(cx);
        Theme::change(ThemeMode::Dark, None, cx);

        cx.spawn(async move |cx| {
            let bounds = Bounds {
                origin: point(px(80.), px(60.)),
                size: size(px(1280.), px(800.)),
            };
            cx.open_window(
                {
                    let mut options = TitleBar::window_options();
                    options.window_bounds = Some(WindowBounds::Windowed(bounds));
                    options.window_min_size = Some(size(px(800.), px(500.)));
                    options.kind = WindowKind::Normal;
                    options.app_id = Some("sisko".into());
                    options
                },
                |window, cx| {
                    window.activate_window();
                    window.set_window_title("Sisko");
                    let (engine, events) = spawn_engine();
                    let view = cx.new(|cx| Workspace::new(engine, events, window, cx));
                    cx.new(|cx| Root::new(view, window, cx))
                },
            )
            .expect("failed to open window");
        })
        .detach();
    });
}

fn init_app_chrome(cx: &mut App) {
    cx.bind_keys([
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-q", Quit, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-,", OpenSettings, None),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("alt-f4", Quit, None),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-q", Quit, None),
    ]);
    cx.on_action(|_: &Quit, cx: &mut App| cx.quit());
    cx.on_action(|_: &ThemeLight, cx: &mut App| {
        Theme::change(ThemeMode::Light, None, cx);
    });
    cx.on_action(|_: &ThemeDark, cx: &mut App| {
        Theme::change(ThemeMode::Dark, None, cx);
    });
    cx.on_action(|_: &ThemeToggle, cx: &mut App| {
        let mode = if cx.theme().mode.is_dark() {
            ThemeMode::Light
        } else {
            ThemeMode::Dark
        };
        Theme::change(mode, None, cx);
    });
    cx.set_menus(vec![
        Menu {
            name: "Sisko".into(),
            items: vec![
                MenuItem::action("Settings…", OpenSettings),
                MenuItem::action("Quit Sisko", Quit),
            ],
            disabled: false,
        },
        Menu {
            name: "View".into(),
            items: vec![
                MenuItem::action("Light", ThemeLight),
                MenuItem::action("Dark", ThemeDark),
                MenuItem::action("Toggle Light/Dark", ThemeToggle),
                MenuItem::separator(),
                MenuItem::action("Auto-size Columns", AutoResizeColumns),
            ],
            disabled: false,
        },
    ]);
    cx.activate(true);
}
