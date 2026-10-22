//! Panel icon and popup of the analog output selector.
//!
//! The panel shows a single icon button; clicking it opens a popup with the
//! three actions. The icon and the highlighted action follow the selected mode,
//! and the popup shows the sink's currently active port for reference.
//!
//! A forced mode is kept in force by watching the sound server's event
//! stream: the driver moves the analog port on its own — the COSMIC panel's
//! own volume control resets the sink's route, and PipeWire picks the other
//! port a couple of milliseconds later — and the port has to be put back
//! within milliseconds, not on the next poll.

use std::time::{Duration, Instant};

use cosmic::app::{Core, Task};
use cosmic::applet::{menu_button, padded_control};
use cosmic::font;
use cosmic::iced::window::{self, Id};
use cosmic::iced::{Alignment, Length, Rectangle, Subscription};
use cosmic::surface::action::{app_popup, destroy_popup};
use cosmic::theme;
use cosmic::widget::{column, divider, icon, space, text, Row};
use cosmic::Element;

use crate::audio::{self, AudioError, OutputMode, SinkState};
use crate::control;
use crate::state;
use crate::watch;
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

/// How often the driver's `Auto-Mute Mode` is re-asserted in a forced mode.
///
/// The driver turns that control back on by itself after a jack event, and a
/// forced port does not survive while it is on. The interval is long enough to
/// be invisible and short enough to put the control back before the user
/// hears the output move.
const AUTO_MUTE_GUARD_INTERVAL: Duration = Duration::from_secs(15);

/// How long the hardware has to settle after the applet wrote the port itself.
///
/// The write is asynchronous: the sink list can still report the previous
/// port right after it. Correcting on that transient is what used to fight
/// the user's own volume control, so the correction waits for the port to
/// settle first.
const APPLY_SETTLE: Duration = Duration::from_millis(500);

/// Minimum time between two refreshes driven by sink events.
///
/// The events arrive in bursts — a single port change produces several of
/// them — and every refresh costs two `pactl` calls. The window keeps that
/// rate bounded without dropping the event that carries the flip.
const EVENT_DEBOUNCE: Duration = Duration::from_millis(50);

/// How many times the port write is attempted before the correction gives up.
///
/// `pactl set-sink-port` is asynchronous: the server accepts the request and
/// PipeWire applies the route change afterwards, so the port read back right
/// after the write can still be the old one. The write is repeated until the
/// read-back shows the forced port, but only a bounded number of times.
const PORT_ATTEMPTS: usize = 3;

/// How many times the level write is attempted before the correction gives up.
///
/// The level write is verified the same way the port write is: the sink is
/// read back and the level is written again if the read-back does not show
/// it, but only a bounded number of times.
const LEVEL_ATTEMPTS: usize = 3;

/// Pause between two attempts of the same write.
///
/// The read-back that follows a write already costs a process round trip, so
/// this is only a short pause: long enough for the sound server to apply
/// what was written, short enough to stay invisible.
const RETRY_PAUSE: Duration = Duration::from_millis(5);

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

/// What the applet has to do about the sink's active port.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Correction {
    /// Nothing to do: the mode does not force a port, or the port in force is
    /// already the active one.
    Keep,
    /// The applet wrote the port itself a moment ago, so the hardware has not
    /// settled yet and the port has to be read again before correcting it.
    Wait,
    /// The driver moved the port: it has to be moved back.
    ///
    /// `level` is the level the sink is showing right now, and it has to go
    /// back with the port: the level the sink shows belongs to the route it
    /// is on, so the port write alone would leave the sink showing the level
    /// stored for the forced port, and the slider would jump under the user's
    /// finger.
    ///
    /// `None` means no level could be read, and then no level is written: a
    /// level is never guessed.
    Move { level: Option<u32> },
}

/// Decides what to do about the active port.
///
/// `since_apply` is the time elapsed since the applet last wrote the port
/// itself. The settle window exists because that write is asynchronous: the
/// sink list can still report the previous port right after it, and a
/// correction on that transient is what used to fight the user's own volume
/// control.
///
/// `level` is the level the sink is showing right now. It is only carried by
/// a [`Correction::Move`], because it is only written when the port moves.
fn correction(
    mode: OutputMode,
    active: Option<&str>,
    since_apply: Option<Duration>,
    level: Option<u32>,
) -> Correction {
    let Some(forced) = mode.forced_port() else {
        return Correction::Keep;
    };

    if active == Some(forced) {
        return Correction::Keep;
    }

    if !settled(since_apply) {
        return Correction::Wait;
    }

    Correction::Move { level }
}

/// Has the hardware settled since the applet's last write?
///
/// A mode that was never applied has nothing to wait for.
fn settled(since_apply: Option<Duration>) -> bool {
    since_apply
        .map(|since| since >= APPLY_SETTLE)
        .unwrap_or(true)
}

/// Is a refresh due, or did one run too recently?
///
/// `elapsed` is the time since the last refresh. A refresh that is too recent
/// is deferred rather than dropped: dropping it would lose the very event
/// that carries the flip.
fn refresh_due(elapsed: Option<Duration>) -> bool {
    elapsed.map(|since| since >= EVENT_DEBOUNCE).unwrap_or(true)
}

/// Is the sink playing through the port this mode forces?
///
/// A forced mode that is in place means the applet's own write landed: the
/// settle window only covers the transient *before* the write lands, so it
/// ends there and a flip that comes later is corrected right away.
fn port_landed(mode: OutputMode, active: Option<&str>) -> bool {
    match (mode.forced_port(), active) {
        (Some(forced), Some(active)) => forced == active,
        _ => false,
    }
}

/// The sound stack the correction writes to.
///
/// The correction is a sequence — write the port, read it back, write the
/// level, read it back — and the sequence is exactly the part that has to be
/// right: a level written before the port write landed lands on the route the
/// sink is not listening through. The trait keeps that sequence testable
/// without a sound server.
trait SoundStack {
    /// Writes the port this mode forces.
    fn write_port(&mut self, sink: &str, mode: OutputMode) -> Result<(), AudioError>;

    /// Writes a level onto the sink's active route.
    fn write_level(&mut self, sink: &str, percent: u32) -> Result<(), AudioError>;

    /// Reads the sink's live state back.
    fn read_state(&mut self) -> Result<SinkState, AudioError>;
}

/// The live sound stack: the `pactl` calls of [`crate::audio`].
struct AudioStack;

impl SoundStack for AudioStack {
    fn write_port(&mut self, sink: &str, mode: OutputMode) -> Result<(), AudioError> {
        audio::apply_port(sink, mode)
    }

    fn write_level(&mut self, sink: &str, percent: u32) -> Result<(), AudioError> {
        audio::set_volume(sink, percent)
    }

    fn read_state(&mut self) -> Result<SinkState, AudioError> {
        audio::sink_state()
    }
}

/// Why a correction could not be completed.
///
/// A failed correction is reported in the popup: the icon would otherwise
/// claim a port the audio is not coming out of.
#[derive(Debug)]
enum CorrectionError {
    /// The port write failed, or the sink could not be read back.
    Port(AudioError),
    /// The port write did not land: the sink kept reporting another port.
    PortNotApplied,
    /// The level write failed, or the sink could not be read back.
    Level(AudioError),
    /// The level write did not land: the sink kept reporting another level.
    LevelNotApplied,
}

impl std::fmt::Display for CorrectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Port(error) => {
                if crate::i18n::is_spanish() {
                    write!(f, "no se pudo forzar el puerto: {error}")
                } else {
                    write!(f, "could not force the port: {error}")
                }
            }
            Self::PortNotApplied => {
                if crate::i18n::is_spanish() {
                    write!(f, "el puerto forzado no llegó a aplicarse")
                } else {
                    write!(f, "the forced port did not apply")
                }
            }
            Self::Level(error) => {
                if crate::i18n::is_spanish() {
                    write!(f, "no se pudo restaurar el nivel: {error}")
                } else {
                    write!(f, "could not restore the level: {error}")
                }
            }
            Self::LevelNotApplied => {
                if crate::i18n::is_spanish() {
                    write!(f, "el nivel no llegó a restaurarse")
                } else {
                    write!(f, "the level did not restore")
                }
            }
        }
    }
}

impl std::error::Error for CorrectionError {}

/// Puts the forced port back and restores the level the sink was showing.
///
/// The writes are a verified, bounded transaction:
///
/// 1. The port is written.
/// 2. The sink is read back. `pactl set-sink-port` is asynchronous — the
///    server accepts the request and PipeWire applies the route afterwards —
///    so the read-back can still report the old port. The port is written
///    again, at most `PORT_ATTEMPTS` times in total.
/// 3. Only once the read-back shows the forced port is the level written:
///    the level observed in this refresh, never a remembered one and never a
///    guessed one.
/// 4. The sink is read back again, and the level is written again if the
///    read-back does not show it, at most `LEVEL_ATTEMPTS` times in total.
///
/// A failed write stops the sequence and is reported: a level written after a
/// failed port write would land on a route the user is not listening through.
fn correct_port(
    stack: &mut impl SoundStack,
    sink: &str,
    mode: OutputMode,
    level: Option<u32>,
) -> Result<(), CorrectionError> {
    // `Auto` forces no port, so nothing is written at all: neither the port
    // nor the level.
    let Some(forced) = mode.forced_port() else {
        return Ok(());
    };

    // The port write, verified by the read-back that follows it.
    let mut applied = false;

    for attempt in 1..=PORT_ATTEMPTS {
        stack
            .write_port(sink, mode)
            .map_err(CorrectionError::Port)?;

        let state = stack.read_state().map_err(CorrectionError::Port)?;

        if matches!(state.port.as_deref(), Some(active) if active == forced) {
            applied = true;
            break;
        }

        if attempt < PORT_ATTEMPTS {
            std::thread::sleep(RETRY_PAUSE);
        }
    }

    if !applied {
        return Err(CorrectionError::PortNotApplied);
    }

    // The level write, verified the same way. Without an observed level there
    // is nothing to restore: a level is never guessed.
    let Some(level) = level else {
        return Ok(());
    };

    let mut restored = false;

    for attempt in 1..=LEVEL_ATTEMPTS {
        stack
            .write_level(sink, level)
            .map_err(CorrectionError::Level)?;

        let state = stack.read_state().map_err(CorrectionError::Level)?;

        if level_landed(state.volume, level) {
            restored = true;
            break;
        }

        if attempt < LEVEL_ATTEMPTS {
            std::thread::sleep(RETRY_PAUSE);
        }
    }

    if !restored {
        return Err(CorrectionError::LevelNotApplied);
    }

    Ok(())
}

/// Has the level the sink shows reached the one that was written?
///
/// The percentage is rounded for the display, so a level that did land is
/// allowed to be off by one.
fn level_landed(shown: Option<u32>, written: u32) -> bool {
    matches!(shown, Some(shown) if shown.abs_diff(written) <= 1)
}

/// Application state.
pub struct AppModel {
    core: Core,
    popup: Option<Id>,
    mode: OutputMode,
    /// Sink port reported by `pactl list sinks` at the last refresh.
    active_port: Option<String>,
    /// Last error from the audio backend or from a failed correction, if any.
    error: Option<String>,
    /// Startup-only retry of the persisted mode, while the sound stack comes up.
    restore: Option<Restore>,
    /// When the applet last wrote the port itself, forced or corrected.
    last_forced_apply: Option<Instant>,
    /// When we last re-asserted that Auto-Mute stays disabled.
    last_auto_mute_guard: Option<Instant>,
    /// When the last refresh ran, to keep the event-driven one bounded.
    last_refresh: Option<Instant>,
    /// Whether a deferred refresh is already pending.
    refresh_scheduled: bool,
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
            last_forced_apply: None,
            last_auto_mute_guard: None,
            last_refresh: None,
            refresh_scheduled: false,
        }
    }
}

impl AppModel {
    /// Re-reads the sink's active port from `pactl`.
    ///
    /// Two things happen here, in order:
    ///
    /// 1. `Auto-Mute Mode` is re-asserted in a forced mode, because the
    ///    driver turns it back on by itself and a forced port does not
    ///    survive that.
    /// 2. A port the driver moved on its own is corrected.
    ///
    /// The correction writes the port and the level the sink is showing right
    /// now, and nothing else. The level the sink shows belongs to the route
    /// it is on, so the port write alone would leave the sink showing the
    /// level stored for the forced port and the slider would jump under the
    /// user's finger. What must *not* be written is a remembered level: the
    /// level the user just chose is the one the sink is showing, and that is
    /// the one that goes back.
    ///
    /// Both writes are verified: the port is only followed by the level once
    /// the sink reports the forced port, and the level is written again if
    /// the read-back does not show it. A failed write is reported in
    /// [`Self::error`] instead of being swallowed.
    fn refresh(&mut self) {
        self.last_refresh = Some(Instant::now());

        let state = audio::sink_state().unwrap_or_default();
        self.active_port = state.port.clone();

        if self.mode.forced_port().is_none() {
            return;
        }

        // The applet's own write has landed once the sink reports the forced
        // port. The settle window only covers the transient *before* the
        // write lands — the sink list can still report the old port right
        // after it — so it ends here, and a flip that comes later is
        // corrected right away instead of waiting for the next poll.
        if port_landed(self.mode, self.active_port.as_deref()) {
            self.last_forced_apply = None;
        }

        // The driver can switch `Auto-Mute Mode` back on by itself, and a
        // forced port does not survive that.
        let guard_due = self
            .last_auto_mute_guard
            .map(|at| at.elapsed() >= AUTO_MUTE_GUARD_INTERVAL)
            .unwrap_or(true);

        if guard_due {
            let _ = audio::ensure_auto_mute(false);
            self.last_auto_mute_guard = Some(Instant::now());
        }

        let since_apply = self.last_forced_apply.map(|at| at.elapsed());
        let decision = correction(
            self.mode,
            self.active_port.as_deref(),
            since_apply,
            state.volume,
        );

        match decision {
            Correction::Keep | Correction::Wait => {}
            Correction::Move { level } => {
                let Some(sink) = state.name.as_deref() else {
                    // There is no sink to address: the correction cannot even
                    // be attempted, and the applet has to say so.
                    self.error = Some(CorrectionError::Port(AudioError::NoAnalogSink).to_string());
                    return;
                };

                match correct_port(&mut AudioStack, sink, self.mode, level) {
                    Ok(()) => {
                        // The port was written and the read-back confirmed it,
                        // so nothing has to settle any more: a flip that comes
                        // later is corrected right away.
                        self.last_forced_apply = None;
                    }
                    Err(error) => {
                        // A failed correction has to be visible: the icon
                        // would otherwise claim a port the audio is not
                        // coming out of.
                        self.error = Some(error.to_string());

                        // The port was written all the same, so the hardware
                        // gets the settle window before the next attempt: a
                        // driver that refuses the port must not be hammered.
                        self.last_forced_apply = Some(Instant::now());
                    }
                }
            }
        }
    }

    /// Reacts to a sink event from the sound server.
    ///
    /// The event only says *that* a sink changed; the port has to be read
    /// again. Events arrive in bursts — one port change produces several of
    /// them — and every refresh costs two `pactl` calls, so a refresh that is
    /// too recent is deferred instead of dropped. Dropping it would lose the
    /// very event that carries the flip.
    fn on_sink_event(&mut self) -> Task<Message> {
        if refresh_due(self.last_refresh.map(|at| at.elapsed())) {
            self.refresh();
            return Task::none();
        }

        if self.refresh_scheduled {
            // A deferred refresh is already pending: it will read the state
            // once the burst is over.
            return Task::none();
        }

        self.refresh_scheduled = true;

        cosmic::task::future(async {
            tokio::time::sleep(EVENT_DEBOUNCE).await;
            Message::DeferredRefresh
        })
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
    /// In `Auto` the driver owns port arbitration; the applet only re-enables
    /// the driver's own control and leaves the port alone.
    fn set_mode(&mut self, mode: OutputMode) {
        self.mode = mode;

        // A mode the user picked wins over a startup restore that is still
        // retrying: the restore would otherwise re-apply the old mode over
        // the new choice a moment later.
        self.restore = None;

        let result = audio::apply(mode);

        if result.is_ok() {
            state::save(mode);

            if mode.forced_port().is_some() {
                // The applet's own write starts the settle window: the sink
                // list can still report the old port right after it.
                self.last_forced_apply = Some(Instant::now());
            }
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
    /// Periodic check to re-assert a forced output mode if the driver flipped it.
    CheckPort,
    /// The sound server reported that a sink changed.
    SinkEvent,
    /// A refresh the event debounce deferred.
    DeferredRefresh,
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
            Message::CheckPort => {
                self.refresh();
            }
            Message::SinkEvent => {
                return self.on_sink_event();
            }
            Message::DeferredRefresh => {
                // The deferred refresh reads the state once the burst of
                // events is over.
                self.refresh_scheduled = false;
                self.refresh();
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

                match audio::apply(restore.mode) {
                    // The state file already records this mode, so the restore
                    // path never writes it: a write on a failed attempt would
                    // overwrite the persisted intent with a value that did not
                    // reach the hardware.
                    Ok(()) => {
                        self.error = None;

                        // The mode is now in force, so the periodic check must
                        // not treat the port as a mismatch right away.
                        if restore.mode.forced_port().is_some() {
                            self.last_forced_apply = Some(Instant::now());
                        }

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
        let control = control::subscription().map(|_| Message::Cycle);
        let poll = cosmic::iced::time::every(Duration::from_secs(2)).map(|_| Message::CheckPort);
        // The sound server's own event stream: a port the driver moved is
        // corrected in tens of milliseconds instead of on the next poll.
        let events = watch::subscription().map(|_| Message::SinkEvent);
        Subscription::batch(vec![control, poll, events])
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

    const LINE_OUT: &str = "analog-output-lineout";
    const HEADPHONES: &str = "analog-output-headphones";

    /// The sink the correction writes to.
    const SINK: &str = "alsa_output.pci-0000_00_1f.3.analog-stereo";

    /// A level the sink could be showing.
    const LEVEL: Option<u32> = Some(62);

    /// Time since the applet's own write, once the hardware has settled.
    fn settled_ago() -> Duration {
        APPLY_SETTLE + Duration::from_millis(1)
    }

    /// A sink state the fake sound stack reports.
    fn state_on(port: &str, volume: Option<u32>) -> SinkState {
        SinkState {
            name: Some(SINK.to_owned()),
            port: Some(port.to_owned()),
            volume,
        }
    }

    /// One call the correction made.
    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Call {
        /// The port was written.
        Port,
        /// The level was written.
        Level(u32),
        /// The sink was read back.
        Read,
    }

    /// A sound stack that records what the correction writes.
    ///
    /// The read-backs are scripted: the fake hands out the states in `reads`
    /// in order and repeats the last one once they run out, which is how a
    /// sink behaves once it stopped changing.
    struct FakeStack {
        /// Every call the correction made, in order.
        calls: Vec<Call>,
        /// The states the read-backs report.
        reads: Vec<SinkState>,
        /// Whether the port write fails.
        port_fails: bool,
        /// Whether the level write fails.
        level_fails: bool,
    }

    impl FakeStack {
        /// A stack whose read-backs report the given states, in order.
        fn new(reads: Vec<SinkState>) -> Self {
            Self {
                calls: Vec::new(),
                reads,
                port_fails: false,
                level_fails: false,
            }
        }

        /// The next state the read-back reports.
        fn next_state(&mut self) -> SinkState {
            self.calls.push(Call::Read);

            if self.reads.len() > 1 {
                self.reads.remove(0)
            } else {
                self.reads.first().cloned().unwrap_or_default()
            }
        }

        /// The levels the correction wrote, in order.
        fn level_writes(&self) -> Vec<u32> {
            self.calls
                .iter()
                .filter_map(|call| match call {
                    Call::Level(level) => Some(*level),
                    _ => None,
                })
                .collect()
        }

        /// The number of port writes the correction made.
        fn port_writes(&self) -> usize {
            self.calls
                .iter()
                .filter(|call| **call == Call::Port)
                .count()
        }
    }

    impl SoundStack for FakeStack {
        fn write_port(&mut self, _sink: &str, _mode: OutputMode) -> Result<(), AudioError> {
            self.calls.push(Call::Port);

            if self.port_fails {
                Err(AudioError::Spawn(std::io::Error::other(
                    "the port write failed",
                )))
            } else {
                Ok(())
            }
        }

        fn write_level(&mut self, _sink: &str, percent: u32) -> Result<(), AudioError> {
            self.calls.push(Call::Level(percent));

            if self.level_fails {
                Err(AudioError::Spawn(std::io::Error::other(
                    "the level write failed",
                )))
            } else {
                Ok(())
            }
        }

        fn read_state(&mut self) -> Result<SinkState, AudioError> {
            Ok(self.next_state())
        }
    }

    #[test]
    fn a_forced_port_that_is_already_active_is_left_alone() {
        // The port is already the forced one: there is nothing to write, not
        // even the level.
        assert_eq!(
            correction(OutputMode::Speakers, Some(LINE_OUT), None, LEVEL),
            Correction::Keep
        );
        assert_eq!(
            correction(OutputMode::Headphones, Some(HEADPHONES), None, LEVEL),
            Correction::Keep
        );
    }

    #[test]
    fn auto_never_corrects_the_port() {
        // `Auto` hands arbitration back to the driver: whatever port the
        // driver picks is the one the user gets, however unexpected it looks.
        // The level is not touched either.
        assert_eq!(
            correction(OutputMode::Auto, Some(LINE_OUT), None, LEVEL),
            Correction::Keep
        );
        assert_eq!(
            correction(OutputMode::Auto, Some(HEADPHONES), None, LEVEL),
            Correction::Keep
        );
    }

    #[test]
    fn a_moved_port_is_corrected_once_the_hardware_settled() {
        // The driver moved the port while a mode was in force: it has to be
        // put back, and the correction must not wait for a second poll.
        assert_eq!(
            correction(
                OutputMode::Speakers,
                Some(HEADPHONES),
                Some(settled_ago()),
                LEVEL
            ),
            Correction::Move { level: LEVEL }
        );
    }

    #[test]
    fn a_port_moved_right_after_our_own_write_is_not_corrected_yet() {
        // The write is asynchronous: the sink list can still report the old
        // port right after it. Correcting on that transient is what used to
        // fight the user's own volume control.
        assert_eq!(
            correction(
                OutputMode::Speakers,
                Some(HEADPHONES),
                Some(Duration::from_millis(10)),
                LEVEL
            ),
            Correction::Wait
        );
    }

    #[test]
    fn a_mode_that_was_never_applied_is_corrected_right_away() {
        assert_eq!(
            correction(OutputMode::Speakers, Some(HEADPHONES), None, LEVEL),
            Correction::Move { level: LEVEL }
        );
    }

    #[test]
    fn a_correction_puts_the_observed_level_back() {
        // PipeWire keeps one level per port, so a port write alone would leave
        // the sink showing the forced port's stale level and the slider would
        // jump. The level the sink is showing right now has to go with it.
        assert_eq!(
            correction(
                OutputMode::Speakers,
                Some(HEADPHONES),
                Some(settled_ago()),
                Some(85)
            ),
            Correction::Move { level: Some(85) }
        );
    }

    #[test]
    fn a_correction_without_an_observed_level_writes_no_volume() {
        // A level is never guessed: without one the correction moves the port
        // and leaves the level alone.
        assert_eq!(
            correction(
                OutputMode::Speakers,
                Some(HEADPHONES),
                Some(settled_ago()),
                None
            ),
            Correction::Move { level: None }
        );
    }

    #[test]
    fn the_settle_window_is_the_only_thing_that_holds_a_correction_back() {
        assert!(settled(None), "nothing was applied, nothing to wait for");
        assert!(settled(Some(APPLY_SETTLE)));
        assert!(!settled(Some(APPLY_SETTLE - Duration::from_millis(1))));
    }

    #[test]
    fn the_settle_window_ends_when_the_forced_port_is_observed() {
        // The applet's own write landed: the window exists only for the
        // transient before it landed, so it ends there and a flip that comes
        // later is corrected right away.
        assert!(port_landed(OutputMode::Speakers, Some(LINE_OUT)));
        assert!(port_landed(OutputMode::Headphones, Some(HEADPHONES)));

        assert!(
            !port_landed(OutputMode::Speakers, Some(HEADPHONES)),
            "the driver moved the port: the write did not land"
        );
        assert!(
            !port_landed(OutputMode::Auto, Some(LINE_OUT)),
            "auto forces no port, so nothing was written"
        );
    }

    #[test]
    fn the_port_is_written_before_the_level() {
        let mut stack = FakeStack::new(vec![state_on(LINE_OUT, LEVEL)]);

        correct_port(&mut stack, SINK, OutputMode::Speakers, LEVEL)
            .expect("the correction must succeed");

        assert_eq!(
            stack.calls,
            vec![Call::Port, Call::Read, Call::Level(62), Call::Read,],
            "the port has to be written — and confirmed — before the level"
        );
    }

    #[test]
    fn a_failed_port_write_leaves_the_level_alone_and_is_reported() {
        // A level written after a failed port write lands on a route the
        // user is not listening through.
        let mut stack = FakeStack::new(vec![state_on(LINE_OUT, LEVEL)]);
        stack.port_fails = true;

        let error = correct_port(&mut stack, SINK, OutputMode::Speakers, LEVEL)
            .expect_err("the failed write must be reported");

        assert!(matches!(error, CorrectionError::Port(_)), "{error}");
        assert!(
            stack.level_writes().is_empty(),
            "the level must not be written after a failed port write"
        );
    }

    #[test]
    fn the_level_is_written_only_after_the_forced_port_is_confirmed() {
        // The first read-back still reports the old port: the write has not
        // landed yet, so the level must not be written onto that route.
        let mut stack =
            FakeStack::new(vec![state_on(HEADPHONES, LEVEL), state_on(LINE_OUT, LEVEL)]);

        correct_port(&mut stack, SINK, OutputMode::Speakers, LEVEL)
            .expect("the correction must succeed");

        assert_eq!(
            stack.calls,
            vec![
                Call::Port,
                Call::Read,
                Call::Port,
                Call::Read,
                Call::Level(62),
                Call::Read,
            ],
            "the level must wait for the read-back that shows the forced port"
        );
    }

    #[test]
    fn the_port_write_gives_up_after_the_attempt_budget() {
        // The sink keeps reporting the old port: the write is repeated a
        // bounded number of times and then reported.
        let mut stack = FakeStack::new(vec![state_on(HEADPHONES, LEVEL)]);

        let error = correct_port(&mut stack, SINK, OutputMode::Speakers, LEVEL)
            .expect_err("the failed correction must be reported");

        assert!(matches!(error, CorrectionError::PortNotApplied), "{error}");
        assert_eq!(stack.port_writes(), PORT_ATTEMPTS);
        assert!(
            stack.level_writes().is_empty(),
            "no level may be written while the port did not land"
        );
    }

    #[test]
    fn the_level_is_written_again_until_the_read_back_shows_it() {
        // The level write is verified too: the read-back keeps reporting the
        // level the forced port had stored until the write lands.
        let mut stack = FakeStack::new(vec![
            state_on(LINE_OUT, Some(35)),
            state_on(LINE_OUT, Some(35)),
            state_on(LINE_OUT, LEVEL),
        ]);

        correct_port(&mut stack, SINK, OutputMode::Speakers, LEVEL)
            .expect("the correction must succeed");

        assert_eq!(
            stack.level_writes(),
            vec![62, 62],
            "the level must be written again when the read-back does not show it"
        );
    }

    #[test]
    fn the_level_write_gives_up_after_the_attempt_budget() {
        // The sink keeps reporting another level: the correction must stop
        // instead of writing forever.
        let mut stack = FakeStack::new(vec![state_on(LINE_OUT, Some(35))]);

        let error = correct_port(&mut stack, SINK, OutputMode::Speakers, LEVEL)
            .expect_err("the failed restore must be reported");

        assert!(matches!(error, CorrectionError::LevelNotApplied), "{error}");
        assert_eq!(stack.level_writes().len(), LEVEL_ATTEMPTS);
    }

    #[test]
    fn a_failed_level_write_is_reported() {
        let mut stack = FakeStack::new(vec![state_on(LINE_OUT, LEVEL)]);
        stack.level_fails = true;

        let error = correct_port(&mut stack, SINK, OutputMode::Speakers, LEVEL)
            .expect_err("the failed write must be reported");

        assert!(matches!(error, CorrectionError::Level(_)), "{error}");
    }

    #[test]
    fn the_correction_writes_no_level_without_an_observed_one() {
        // A level is never guessed: without one the correction moves the port
        // and leaves the level alone.
        let mut stack = FakeStack::new(vec![state_on(LINE_OUT, LEVEL)]);

        correct_port(&mut stack, SINK, OutputMode::Speakers, None)
            .expect("the correction must succeed");

        assert!(
            stack.level_writes().is_empty(),
            "without an observed level there is nothing to restore"
        );
    }

    #[test]
    fn auto_writes_nothing_at_all() {
        // `Auto` hands arbitration back to the driver: neither the port nor
        // the level is written.
        let mut stack = FakeStack::new(vec![state_on(HEADPHONES, LEVEL)]);

        correct_port(&mut stack, SINK, OutputMode::Auto, LEVEL).expect("auto must not fail");

        assert!(
            stack.calls.is_empty(),
            "auto must not write anything: {:?}",
            stack.calls
        );
    }

    #[test]
    fn a_refresh_that_is_too_recent_is_deferred_instead_of_dropped() {
        // Dropping the event would lose the flip: the volume-change event and
        // the flip arrive a couple of milliseconds apart.
        assert!(refresh_due(None), "the first refresh is always due");
        assert!(!refresh_due(Some(Duration::from_millis(1))));
        assert!(refresh_due(Some(EVENT_DEBOUNCE)));
    }

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
