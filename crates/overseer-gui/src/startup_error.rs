//! The window shown when the GUI cannot reach the daemon at startup.
//!
//! Launched from Finder there is no terminal to print to, so the error has
//! to be on screen. Quitting (the button, Cmd+Q or closing the window) exits
//! with status 1.

use gpui::{
    actions, div, prelude::*, px, rgb, size, App, Application, Bounds, Context, KeyBinding,
    SharedString, TitlebarOptions, Window, WindowBounds, WindowOptions,
};

use crate::app::colors;

actions!(overseer_startup, [QuitWithError]);

/// What to tell the user to do, given the log directory.
pub fn advice(log_dir: &std::path::Path) -> String {
    let log_dir = tilde(log_dir);
    format!(
        "Overseer keeps its sessions in a background daemon and could not reach it. \
         The daemon log is in {log_dir}/daemon.log. To start the daemon, run \
         `overseer daemon` in a terminal, or `make install` from the Overseer \
         checkout to reinstall the LaunchAgent. Then open Overseer again."
    )
}

/// `path` with the home directory shown as `~`.
fn tilde(path: &std::path::Path) -> String {
    match std::env::var_os("HOME")
        .and_then(|h| path.strip_prefix(h).ok().map(|r| r.display().to_string()))
    {
        Some(rest) if rest.is_empty() => "~".into(),
        Some(rest) => format!("~/{rest}"),
        None => path.display().to_string(),
    }
}

struct StartupError {
    message: SharedString,
    advice: SharedString,
}

impl Render for StartupError {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .bg(rgb(colors::BG))
            .text_color(rgb(colors::TEXT))
            .p(px(24.))
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(
                div()
                    .text_size(px(18.))
                    .text_color(rgb(colors::RED))
                    .child("Overseer could not start"),
            )
            .child(
                div()
                    .flex_1()
                    .overflow_hidden()
                    .flex()
                    .flex_col()
                    .gap(px(12.))
                    .child(div().child(self.message.clone()))
                    .child(
                        div()
                            .text_color(rgb(colors::TEXT_DIM))
                            .child(self.advice.clone()),
                    ),
            )
            .child(
                div().flex().justify_end().child(
                    div()
                        .id("quit")
                        .px_3()
                        .py_1()
                        .rounded(px(5.))
                        .border_1()
                        .border_color(rgb(colors::BUTTON_BORDER))
                        .bg(rgb(colors::ACCENT_BG))
                        .text_color(rgb(colors::ACCENT))
                        .cursor_pointer()
                        .hover(|s| s.bg(rgb(colors::CARD_HOVER)))
                        .child("Quit")
                        .on_click(cx.listener(|_, _, _, _| quit())),
                ),
            )
    }
}

fn quit() -> ! {
    std::process::exit(1)
}

/// Shows `message` and `advice` in a window until the user quits, then exits
/// the process with status 1. Never returns.
pub fn run_startup_error(message: String, advice: String) -> ! {
    Application::new().run(move |cx: &mut App| {
        cx.bind_keys([KeyBinding::new("cmd-q", QuitWithError, None)]);
        cx.on_action(|_: &QuitWithError, _| quit());
        cx.set_menus(vec![gpui::Menu {
            name: "Overseer".into(),
            items: vec![gpui::MenuItem::action("Quit Overseer", QuitWithError)],
        }]);
        let bounds = Bounds::centered(None, size(px(640.), px(460.)), cx);
        let opened = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("Overseer".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |_, cx| {
                cx.new(|_| StartupError {
                    message: message.into(),
                    advice: advice.into(),
                })
            },
        );
        if opened.is_err() {
            quit();
        }
        cx.on_window_closed(|_| quit()).detach();
        cx.activate(true);
    });
    quit()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advice_names_the_log_and_the_way_out() {
        let home = std::env::var("HOME").unwrap();
        let a = advice(&std::path::Path::new(&home).join("Library/Logs/Overseer"));
        assert!(a.contains("~/Library/Logs/Overseer/daemon.log"), "{a}");
        assert!(a.contains("overseer daemon"), "{a}");
        assert!(a.contains("open Overseer again"), "{a}");
    }
}
