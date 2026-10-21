//! Panel icon and popup of the analog output selector.
//!
//! The panel shows a single icon button; clicking it opens a popup with the
//! three actions. The icon and the highlighted action follow the selected mode,
//! and the popup shows the sink's currently active port for reference.

use std::time::Duration;

use cosmic::app::{Core, Task};
use cosmic::applet::{menu_button, padded_control};
use cosmic::font;
use cosmic::iced::window::{self, Id};
use cosmic::iced::{Alignment, Length, Rectangle, Subscription};
use cosmic::surface::action::{app_popup, destroy_popup};
use cosmic::theme;
use cosmic::widget::{column, divider, icon, space, text, Row};
use cosmic::Element;

use crate::audio::{self, OutputMode};
use crate::control;
use crate::state;

/// Reverse-domain identifier of the applet.
///
/// The underscore in the last segment is deliberate: Flatpak rejects an app ID
/// whose hyphen falls outside the final segment, so the GitHub handle
/// (`Nahel_A24`) is folded into the final segment with an underscore.
const APP_ID: &str = "io.github.Nahel_A24.CosmicExtAppletAudioSwitch";

/// The modes offered in the popup, in display order.
const MODES: [OutputMode; 3] = [
    OutputMode::Speakers,
    OutputMode::Headphones,
    OutputMode::Auto,
];

/// How many times a startup restore is attempted before giving up.
///
/// The panel is launched independently of the sound stack, so it can come up
/// before PipeWire/WirePlumber have recreated the sink. `audio::apply` needs an
/// analog sink to be discoverable, so the restore is retried instead of being
/// lost to a race that resolves a moment later.
const RESTORE_ATTEMPTS: u8 = 25;

/// Delay between restore attempts.
///
/// `RESTORE_ATTEMPTS * RESTORE_DELAY` is roughly a ten-second budget: long
/// enough for a slow boot to bring the sink up, short enough to stop long
/// before the user would notice the retries. The wait is a timer inside the
/// task, never a blocking sleep on the UI path.
const RESTORE_DELAY: Duration = Duration::from_millis(400);

/// A pending startup restore of the persisted output mode.
///
/// Armed only from [`AppModel::init`]; user actions never create one, so a
/// restore can never override a mode the user picked after startup.
struct Restore {
    mode: OutputMode,
    attempts_left: u8,
}

impl Restore {
    /// Arms a restore with the full attempt budget.
    fn new(mode: OutputMode) -> Self {
        Self {
            mode,
            attempts_left: RESTORE_ATTEMPTS,
        }
    }

    /// Records one failed attempt, or gives up once the budget is exhausted.
    ///
    /// Kept as a pure decision so the retry bookkeeping — decrement to
    /// exhaustion, then stop — is testable without a sound server or a timer.
    /// `checked_sub` also makes an accidental zero budget a clean give-up
    /// instead of an underflow.
    fn on_failure(self) -> Option<Self> {
        let attempts_left = self.attempts_left.checked_sub(1)?;
        (attempts_left > 0).then_some(Self {
            mode: self.mode,
            attempts_left,
        })
    }
}

/// Application state.
pub struct AppModel {
    core: Core,
    popup: Option<Id>,
    mode: OutputMode,
    /// Sink port reported by `pactl list sinks` at the last refresh.
    active_port: Option<String>,
    /// Last error from the audio backend, if any.
    error: Option<String>,
    /// Startup-only retry of the persisted mode, while the sound stack comes up.
    restore: Option<Restore>,
}

impl Default for AppModel {
    fn default() -> Self {
        Self {
            core: Core::default(),
            popup: None,
            mode: OutputMode::Auto,
            active_port: None,
            error: None,
            restore: None,
        }
    }
}

impl AppModel {
    /// Re-reads the sink's active port from `pactl`.
    fn refresh(&mut self) {
        self.active_port = audio::active_port().ok().flatten();
    }

    /// Selects a mode, applies it and refreshes the reported port.
    ///
    /// Shared by the popup rows and the `--cycle` shortcut so both leave the
    /// applet in exactly the same state; the icon and the check mark follow
    /// `self.mode`, so a path that applied a mode without updating it would show
    /// a stale selection.
    ///
    /// The choice is persisted only once the hardware actually accepted it: the
    /// state file records the mode the user is really in, so a failed apply
    /// leaves the previously saved intent untouched instead of promoting an
    /// intent that never took effect.
    ///
    /// In `Auto` the driver owns port arbitration; do not call `audio::apply`
    /// on user-triggered changes as it can flip ports in response to volume/sink
    /// events on some HDA codecs.
    fn set_mode(&mut self, mode: OutputMode) {
        self.mode = mode;
        let result = match mode {
            OutputMode::Auto => Ok(()),
            other => audio::apply(other),
        };

        if result.is_ok() {
            state::save(mode);
        }

        self.error = result.err().map(|err| err.to_string());
        self.refresh();
    }

    /// Builds the popup body.
    ///
    /// The structure mirrors the selector popups System76 ships in
    /// `cosmic-applet-input-sources` and `cosmic-applet-audio`: a plain column of
    /// `menu_button` rows at padding `[8, 0]`, the active row carrying the
    /// check mark and set in bold, then a padded divider and a caption. A
    /// generic `button` must not be used here — `menu_button` is what applies
    /// the applet's own menu-item styling (`Button::AppletMenu` plus
    /// `menu_control_padding`), with no corner radius and no accent fill.
    fn popup_content(&self) -> Element<'_, Message> {
        let spacing = theme::spacing();

        let mut content = column::with_capacity(MODES.len() + 2).padding([8, 0]);

        for mode in MODES {
            let active = self.mode == mode;

            // Mark the active mode with a leading check mark, drawn exactly the
            // way COSMIC draws a checked entry in its own menus: the glyph is
            // `object-select-symbolic` at 16px tinted with the accent text
            // colour, and every inactive row reserves the same 16px box so all
            // labels stay aligned. Note that `.selected()` would be a no-op
            // here, because the `AppletMenu` style ignores the selection flag.
            let mark: Element<'_, Message> = if active {
                icon::from_name("object-select-symbolic")
                    .size(16)
                    .icon()
                    .class(theme::Svg::custom(|theme| {
                        cosmic::iced::widget::svg::Style {
                            color: Some(theme.cosmic().accent_text_color().into()),
                        }
                    }))
                    .width(Length::Fixed(16.0))
                    .into()
            } else {
                space::horizontal().width(Length::Fixed(16.0)).into()
            };

            // Bold is kept as the secondary cue, matching the sibling applet
            // `cosmic-applet-input-sources`, which marks its active entry in
            // bold instead of with a glyph.
            let font = if active {
                font::bold()
            } else {
                font::default()
            };

            let row = Row::with_capacity(3)
                .push(mark)
                .push(space::horizontal().width(spacing.space_xxs))
                .push(text::body(mode.label()).font(font))
                .align_y(Alignment::Center);

            content = content.push(menu_button(row).on_press(Message::SelectMode(mode)));
        }

        // Report the port the driver actually has live. This is the only visible
        // difference between a forced mode that stuck and one the driver
        // overrode, so it stays in the popup.
        if let Some(port) = self.active_port.as_deref().map(audio::port_label) {
            content = content.push(
                padded_control(divider::horizontal::default())
                    .padding([spacing.space_xxs, spacing.space_s]),
            );
            content = content.push(padded_control(text::caption(format!(
                "Salida activa: {port}"
            ))));
        }

        if let Some(error) = &self.error {
            content = content.push(padded_control(text::caption(error.as_str())));
        }

        content.into()
    }
}

/// Messages handled by the applet.
#[derive(Clone, Debug)]
pub enum Message {
    /// The popup window was closed.
    PopupClosed(Id),
    /// The user picked an output mode.
    SelectMode(OutputMode),
    /// The `--cycle` shortcut asked for the next mode, relayed from the control
    /// socket.
    Cycle,
    /// A surface action from the applet (popup creation / destruction).
    Surface(cosmic::surface::Action<Message>),
    /// One startup attempt at re-applying the persisted mode.
    RestoreAttempt,
}

impl cosmic::Application for AppModel {
    type Executor = cosmic::SingleThreadExecutor;
    type Flags = ();
    type Message = Message;
    const APP_ID: &'static str = APP_ID;

    fn core(&self) -> &Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    fn init(core: Core, _flags: Self::Flags) -> (Self, Task<Message>) {
        let mut app = AppModel {
            core,
            ..Default::default()
        };
        app.refresh();

        // Restore the user's last accepted mode. Setting `self.mode` right away
        // keeps the icon honest from the first frame (in `Auto` the icon follows
        // the active port, so `Automático` would otherwise look like
        // `Parlantes`). The restore itself is retried because the panel can
        // start before the sound stack is ready; the first attempt runs
        // immediately. With nothing persisted (a fresh install) no state is
        // touched and no hardware call is made.
        if let Some(mode) = state::load() {
            app.mode = mode;
            app.restore = Some(Restore::new(mode));
            return (app, cosmic::task::message(Message::RestoreAttempt));
        }

        (app, Task::none())
    }

    /// Closes the popup on Escape.
    ///
    /// The popup grabs the keyboard (`get_popup_settings` sets `grab: true`), so
    /// it receives key events while open, and the keyboard navigation
    /// subscription in `cosmic::applet::run` turns Escape into this call.
    ///
    /// The id is taken as well as destroyed, so the state stays consistent even
    /// if the surface disappears without a close request. A close request that
    /// does arrive only clears an already-cleared field.
    fn on_escape(&mut self) -> Task<Message> {
        match self.popup.take() {
            Some(id) => cosmic::task::message(cosmic::Action::Surface(destroy_popup(id))),
            None => Task::none(),
        }
    }

    fn on_close_requested(&self, id: window::Id) -> Option<Message> {
        Some(Message::PopupClosed(id))
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::PopupClosed(id) => {
                if self.popup.as_ref() == Some(&id) {
                    self.popup = None;
                }
            }
            Message::SelectMode(mode) => {
                self.set_mode(mode);
            }
            Message::Cycle => {
                self.set_mode(self.mode.next());
            }
            Message::Surface(action) => {
                return cosmic::task::message(cosmic::Action::Surface(action));
            }
            Message::RestoreAttempt => {
                // A stray attempt — never armed, or already resolved — must do
                // nothing. Taking the state first makes the handler idempotent.
                let Some(restore) = self.restore.take() else {
                    return Task::none();
                };

                match match restore.mode {
                    OutputMode::Auto => Ok(()),
                    other => audio::apply(other),
                } {
                    // The state file already records this mode, so the restore
                    // path never writes it: a write on a failed attempt would
                    // overwrite the persisted intent with a value that did not
                    // reach the hardware.
                    Ok(()) => {
                        self.error = None;
                        self.refresh();
                    }
                    Err(error) => match restore.on_failure() {
                        // Still inside the budget: wait, then try again. The
                        // wait is a timer inside the task, so the UI stays
                        // responsive while the sound stack finishes starting.
                        Some(next) => {
                            self.restore = Some(next);
                            self.refresh();
                            return cosmic::task::future(async {
                                tokio::time::sleep(RESTORE_DELAY).await;
                                Message::RestoreAttempt
                            });
                        }
                        // Budget exhausted: surface the last error (the popup
                        // renders it as a caption) and stop retrying.
                        None => {
                            self.error = Some(error.to_string());
                        }
                    },
                }
            }
        }
        Task::none()
    }

    fn subscription(&self) -> Subscription<Message> {
        // Each socket request becomes `Message::Cycle`, applied in-process so the
        // panel icon and the popup check mark follow the change.
        control::subscription().map(|_| Message::Cycle)
    }

    fn view(&self) -> Element<'_, Message> {
        let have_popup = self.popup;
        let btn = self
            .core
            .applet
            .icon_button(self.mode.effective_icon_name(self.active_port.as_deref()))
            .on_press_with_rectangle(move |offset, bounds| {
                if let Some(id) = have_popup {
                    Message::Surface(destroy_popup(id))
                } else {
                    Message::Surface(app_popup::<AppModel>(
                        |_| Default::default(),
                        move |state: &mut AppModel| {
                            let new_id = Id::unique();
                            state.popup = Some(new_id);
                            // Position the popup under the panel icon.
                            let mut popup_settings = state.core.applet.get_popup_settings(
                                state.core.main_window_id().unwrap(),
                                new_id,
                                None,
                                None,
                                None,
                            );

                            popup_settings.positioner.anchor_rect = Rectangle {
                                x: (bounds.x - offset.x) as i32,
                                y: (bounds.y - offset.y) as i32,
                                width: bounds.width as i32,
                                height: bounds.height as i32,
                            };

                            popup_settings
                        },
                        Some(Box::new(move |state: &AppModel| {
                            let content_list = state.popup_content();
                            Element::from(state.core.applet.popup_container(content_list))
                                .map(cosmic::Action::App)
                        })),
                    ))
                }
            });

        Element::from(self.core.applet.applet_tooltip::<Message>(
            btn,
            "Salida de audio",
            self.popup.is_some(),
            Message::Surface,
            None,
        ))
    }

    fn view_window(&self, _id: Id) -> Element<'_, Message> {
        // Popups are drawn by the `app_popup` view closure above; this default
        // is not expected to be reached. Return an empty element instead of
        // panicking like the trait's default implementation does.
        text("").into()
    }

    fn style(&self) -> Option<cosmic::iced::theme::Style> {
        Some(cosmic::applet::style())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_closes_the_open_popup() {
        use cosmic::Application;

        let mut app = AppModel {
            popup: Some(Id::unique()),
            ..AppModel::default()
        };

        let _ = app.on_escape();

        // Clearing here (not only on the close request) is what lets the icon
        // reopen the popup: a stale id would make the next click destroy a
        // surface that is already gone instead of opening a new one.
        assert_eq!(app.popup, None, "Escape must forget the open popup");
    }

    #[test]
    fn escape_with_no_popup_is_a_no_op() {
        use cosmic::Application;

        let mut app = AppModel::default();

        let _ = app.on_escape();

        assert_eq!(app.popup, None);
    }

    #[test]
    fn next_matches_the_popup_display_order() {
        // `next` lives in `audio`, but its contract is the order the popup
        // renders here: three steps must walk `MODES` and wrap. If either list
        // changes, this fails instead of the shortcut silently diverging from
        // what the popup shows.
        for (index, mode) in MODES.iter().enumerate() {
            let expected = MODES[(index + 1) % MODES.len()];
            assert_eq!(
                mode.next(),
                expected,
                "MODES disagrees with OutputMode::next"
            );
        }
    }

    #[test]
    fn restore_gives_up_after_the_budget_is_exhausted() {
        // One unit of budget is one attempt: starting from RESTORE_ATTEMPTS, the
        // loop must yield RESTORE_ATTEMPTS - 1 retries and then stop.
        let mut current = Some(Restore::new(OutputMode::Speakers));
        let mut attempts = 0;

        while let Some(restore) = current.take() {
            attempts += 1;
            assert!(
                attempts <= RESTORE_ATTEMPTS as usize,
                "the retry loop must terminate within its budget"
            );
            current = restore.on_failure();
        }

        assert_eq!(
            attempts, RESTORE_ATTEMPTS as usize,
            "the budget must be spent entirely, one attempt per unit"
        );
    }

    #[test]
    fn restore_stops_at_the_last_attempt_without_underflow() {
        // The final attempt must give up rather than return a borrow of zero.
        let last = Restore {
            mode: OutputMode::Auto,
            attempts_left: 1,
        };
        assert!(last.on_failure().is_none(), "the last attempt must give up");

        // A zero budget (which the code never arms) must not underflow either.
        let empty = Restore {
            mode: OutputMode::Auto,
            attempts_left: 0,
        };
        assert!(
            empty.on_failure().is_none(),
            "a zero budget must give up, not underflow"
        );
    }
}
