//! Audio backend seam.
//!
//! Everything that touches the sound stack lives behind this module so the I/O
//! surface stays small. Two independent pieces have to agree for a forced
//! output to actually stick:
//!
//! 1. The PipeWire sink port of the analog HDA sink, moved with `pactl`.
//! 2. The HDA driver's own jack arbitration control, `Auto-Mute Mode`.
//!
//! The driver keeps an ALSA control called `Auto-Mute Mode` that decides by
//! itself whether the rear line-out stays muted while headphones are plugged
//! in. While that control is `Enabled`, any port set through PipeWire is
//! silently overridden by the driver — this is exactly why a plain
//! `pactl set-sink-port` from other tools does not persist. Both forced modes
//! therefore disable it through the `alsa` crate; `Auto` re-enables it and
//! hands arbitration back to the driver. That is the whole reason this applet
//! exists.
//!
//! The ALSA element and item names below come from the kernel driver and are
//! not localized, unlike `pactl` output, so they are matched verbatim.

use std::process::Command;

use alsa::mixer::{Mixer, SelemChannelId, SelemId};

/// Sink port that routes to the rear line-out (the speakers).
const PORT_LINE_OUT: &str = "analog-output-lineout";

/// Sink port that routes to the front headphone jack.
const PORT_HEADPHONES: &str = "analog-output-headphones";

/// Port-name prefix shared by every analog output of the HDA sink. Used to tell
/// an analog sink apart from HDMI/USB sinks when discovering it at runtime.
const ANALOG_PORT_PREFIX: &str = "analog-output-";

/// Name of the HDA driver control that arbitrates speakers vs headphones.
const AUTO_MUTE_CONTROL: &str = "Auto-Mute Mode";

/// Channel names that mark a `pactl` line as carrying a live volume.
///
/// The volume label itself is translated, but PulseAudio prints the channel
/// names untranslated, so they are the locale-independent anchor. The base
/// volume line has none of them and is skipped.
const VOLUME_CHANNEL_MARKERS: [&str; 3] = ["front-left:", "front-right:", "mono:"];

/// Display name for the sink port the driver currently has live.
///
/// The popup caption is user-facing, so the two ports this applet can force get
/// their Spanish mode labels back instead of the raw ALSA identifier. An
/// unexpected port is passed through unchanged rather than hidden, so the
/// caption never claims a state the hardware is not in.
pub fn port_label(port: &str) -> &str {
    match port {
        PORT_HEADPHONES => "Auriculares",
        PORT_LINE_OUT => "Parlantes",
        other => other,
    }
}

/// The three selectable output modes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputMode {
    /// Force the speakers regardless of the headphone jack.
    Speakers,
    /// Force the headphones regardless of the driver's arbitration.
    Headphones,
    /// Hand the decision back to the driver / WirePlumber.
    Auto,
}

impl OutputMode {
    /// User-facing label.
    ///
    /// These three strings are the product's Spanish UI labels; every visible
    /// string in the popup is Spanish, while code, comments and identifiers
    /// stay English.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Speakers => "Parlantes",
            Self::Headphones => "Auriculares",
            Self::Auto => "Automático",
        }
    }

    /// Machine-readable token persisted to the state file.
    ///
    /// These tokens are English identifiers on purpose, unlike [`Self::label`]:
    /// the token is a persisted file format that must stay stable across
    /// releases and independent of the interface language, while the label is
    /// Spanish UI copy that may be reworded at any time. Persisting the label
    /// would couple the on-disk format to the product's visible wording, so a
    /// translation change could silently invalidate every saved mode.
    pub const fn token(self) -> &'static str {
        match self {
            Self::Speakers => "speakers",
            Self::Headphones => "headphones",
            Self::Auto => "auto",
        }
    }

    /// Parses a token written by [`Self::token`].
    ///
    /// An unknown token returns `None` instead of an error: callers treat a
    /// missing, unreadable, or unrecognised file identically ("nothing was
    /// persisted") and must never fail to start because of it.
    ///
    /// This is not `const`, unlike [`Self::token`] and [`Self::label`]: matching
    /// on `str` is not allowed in constant functions yet
    /// (rust-lang/rust#143874), so the parser runs at the one place it is used
    /// (startup) rather than paying for a hand-rolled byte comparison.
    pub fn from_token(token: &str) -> Option<Self> {
        match token {
            "speakers" => Some(Self::Speakers),
            "headphones" => Some(Self::Headphones),
            "auto" => Some(Self::Auto),
            _ => None,
        }
    }

    /// Panel icon for this mode.
    ///
    /// `speaker-symbolic` (singular, without sound waves) is deliberate: in the
    /// Cosmic icon theme `audio-speakers-symbolic` and `audio-speaker-symbolic`
    /// are the exact same drawing as `audio-volume-high-symbolic`, the stock
    /// audio applet icon, so they are indistinguishable in the panel.
    ///
    /// In `Auto` mode the driver chooses the port; the UI should reflect the
    /// current hardware state when known, so callers should prefer
    /// `effective_icon_name` instead of this raw mapping.
    #[allow(dead_code)]
    pub const fn icon_name(self) -> &'static str {
        match self {
            Self::Headphones => "audio-headphones-symbolic",
            Self::Speakers | Self::Auto => "speaker-symbolic",
        }
    }

    /// Effective icon name considering the current active port.
    ///
    /// When in `Auto` mode, the driver decides the port; showing the active
    /// port's icon matches the native behavior more closely than always showing
    /// the speakers icon.
    pub fn effective_icon_name(self, active_port: Option<&str>) -> &'static str {
        match self {
            Self::Headphones => "audio-headphones-symbolic",
            Self::Speakers => "speaker-symbolic",
            Self::Auto => {
                if let Some(port) = active_port {
                    if port == PORT_HEADPHONES {
                        return "audio-headphones-symbolic";
                    }
                }
                "speaker-symbolic"
            }
        }
    }

    /// Sink port this mode forces, if any. `Auto` lets the driver choose.
    pub(crate) const fn forced_port(self) -> Option<&'static str> {
        match self {
            Self::Speakers => Some(PORT_LINE_OUT),
            Self::Headphones => Some(PORT_HEADPHONES),
            Self::Auto => None,
        }
    }

    /// The next mode in the popup's display order, wrapping around.
    ///
    /// The order matches the `MODES` list the popup renders, so the `--cycle`
    /// shortcut walks the same list the user sees: `Speakers → Headphones →
    /// Auto → Speakers`. The step is unconditional — it never inspects the
    /// headphone jack or skips a mode — so three steps always return to the
    /// starting mode.
    pub const fn next(self) -> Self {
        match self {
            Self::Speakers => Self::Headphones,
            Self::Headphones => Self::Auto,
            Self::Auto => Self::Speakers,
        }
    }
}

/// Errors produced by the audio backend.
#[derive(Debug)]
pub enum AudioError {
    /// `pactl` could not be spawned at all.
    Spawn(std::io::Error),
    /// `pactl` ran but exited with a non-zero status.
    Command {
        args: String,
        status: std::process::ExitStatus,
        stderr: String,
    },
    /// No sink exposing `analog-output-*` ports was found.
    NoAnalogSink,
    /// The mixer is reachable but has no `Auto-Mute Mode` control.
    AutoMuteControlMissing,
    /// The control exists but exposes neither a `Disabled` nor an `Enabled` item.
    AutoMuteItemMissing { labels: Vec<String> },
    /// An ALSA mixer call failed.
    Alsa(alsa::Error),
}

impl std::fmt::Display for AudioError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(err) => write!(f, "no se pudo ejecutar pactl: {err}"),
            Self::Command {
                args,
                status,
                stderr,
            } => write!(f, "pactl {args} falló ({status}): {}", stderr.trim()),
            Self::NoAnalogSink => {
                write!(f, "no se encontró ningún sink con puertos analog-output-*")
            }
            Self::AutoMuteControlMissing => {
                write!(
                    f,
                    "el control ALSA '{AUTO_MUTE_CONTROL}' no existe en la tarjeta"
                )
            }
            Self::AutoMuteItemMissing { labels } => write!(
                f,
                "el control '{AUTO_MUTE_CONTROL}' no ofrece 'Disabled' ni 'Enabled' (items: {})",
                labels.join(", ")
            ),
            Self::Alsa(err) => write!(f, "error de ALSA: {err}"),
        }
    }
}

impl std::error::Error for AudioError {}

/// Applies an output mode to the live sound stack.
pub fn apply(mode: OutputMode) -> Result<(), AudioError> {
    apply_preserving(mode, None)
}

/// Applies an output mode, carrying a known volume level across the switch.
///
/// `volume` is the level the user chose. `None` means "use whatever the sink
/// is playing right now", which is what a switch the user just asked for
/// wants. The correction path passes the level it saw *before* the driver
/// moved the port: by then the sink already shows the other port's stored
/// level, so capturing it here would restore the wrong number.
pub fn apply_preserving(mode: OutputMode, volume: Option<u32>) -> Result<(), AudioError> {
    // A single discovery feeds both halves of a mode change: the sink name
    // drives the port and the sink's own ALSA card drives the mixer. It also
    // runs before anything is written, so a machine with no analog sink fails
    // without having flipped `Auto-Mute Mode` on some unrelated codec first.
    let sink = discover_analog_sink()?;

    match mode.forced_port() {
        Some(port) => {
            // A forced port only survives if the driver stops arbitrating:
            // leave `Auto-Mute Mode` enabled and the driver re-mutes the
            // line-out (or moves the port back) as soon as the jack changes.
            set_auto_mute(false, sink.alsa_card)?;

            // PipeWire stores one volume per port, so moving the port swaps the
            // visible volume for the level stored on the target port. The
            // volume is captured before the switch and written back onto the
            // new port afterwards, so forcing an output never changes how loud
            // the user hears it.
            let volume = volume.or(sink.volume);
            set_sink_port_on(&sink.name, port)?;

            if let Some(percent) = volume {
                let _ = set_sink_volume_on(&sink.name, percent);
            }

            Ok(())
        }
        // `Auto` hands arbitration back to the driver: `Auto-Mute Mode` is
        // re-enabled and the port is left exactly where it is. Forcing a port
        // here would fight the driver on the next jack event, and switching
        // ports behind the user's back is what makes the volume jump.
        //
        // A codec without that control is already handing arbitration to the
        // driver, so there is nothing to give back and the mode still holds.
        None => match set_auto_mute(true, sink.alsa_card) {
            Err(AudioError::AutoMuteControlMissing) => Ok(()),
            other => other,
        },
    }
}

/// Runs `pactl` with a forced C locale.
///
/// `pactl` localizes its field names (on a Spanish system `list sinks` prints
/// `Puerto Activo:` instead of `Active Port:`), so any parsing must not depend
/// on the caller's locale. The Python implementation this applet replaces
/// forced `LC_ALL=C` for the same reason.
fn pactl(args: &[&str]) -> Result<std::process::Output, AudioError> {
    Command::new("pactl")
        .args(args)
        .env("LC_ALL", "C")
        .output()
        .map_err(AudioError::Spawn)
}

/// Runs `pactl` and returns its stdout, mapping a non-zero exit to an error.
fn pactl_stdout(args: &[&str]) -> Result<String, AudioError> {
    let output = pactl(args)?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(AudioError::Command {
            args: args.join(" "),
            status: output.status,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// Reads the default sink name from `pactl get-default-sink`.
///
/// A missing default is not an error: discovery falls back to the first analog
/// sink it finds.
fn default_sink() -> Result<Option<String>, AudioError> {
    let output = pactl(&["get-default-sink"])?;

    if !output.status.success() {
        return Ok(None);
    }

    let name = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    Ok((!name.is_empty()).then_some(name))
}

/// One sink parsed out of `pactl list sinks`.
#[derive(Debug, Default, PartialEq, Eq)]
struct Sink {
    /// PipeWire node name, e.g. `alsa_output.pci-0000_00_1f.3.analog-stereo`.
    name: String,
    /// Port names exposed by this sink, e.g. `analog-output-lineout`.
    ports: Vec<String>,
    /// Port currently active on this sink, when the list reports one.
    active_port: Option<String>,
    /// ALSA card index backing this sink, from its `alsa.card` property.
    ///
    /// This is what lets `Auto-Mute Mode` be reached on the *right* codec when a
    /// machine has more than one analog codec: the sink itself says which card it
    /// plays through, so the codec does not have to be guessed by scan order.
    /// It covers USB and other non-PCI buses, which carry no PCI address in the
    /// node name, and nothing here needs `/sys`.
    alsa_card: Option<i32>,
    /// Current sink volume, as a percentage of the base volume.
    ///
    /// PipeWire keeps one volume per port, so this value belongs to the port
    /// that is active right now. Moving the port swaps it for the value stored
    /// on the target port, which is why the applet has to carry the volume
    /// across a port switch.
    volume: Option<u32>,
}

/// Does this sink expose at least one `analog-output-*` port?
fn is_analog(sink: &Sink) -> bool {
    sink.ports
        .iter()
        .any(|port| port.starts_with(ANALOG_PORT_PREFIX))
}

/// Reads a sink name from a line, if the line carries one.
///
/// The `node.name` PipeWire property is the primary anchor because its key is
/// never localized; the C-locale `Name:` header is accepted as a fallback. Both
/// carry the same value, so the caller de-duplicates them.
fn sink_name(line: &str) -> Option<String> {
    if let Some(rest) = line.strip_prefix("node.name = ") {
        return Some(rest.trim().trim_matches('"').to_owned());
    }
    if let Some(rest) = line.strip_prefix("Name: ") {
        return Some(rest.trim().to_owned());
    }
    None
}

/// Reads the ALSA card index from a line, if the line carries that property.
///
/// The match is an exact prefix on purpose: PipeWire prints `alsa.card`,
/// `api.alsa.card` and `api.alsa.pcm.card` for the same sink, and only the first
/// is the sink's own card. `alsa.card_name` is likewise not a match.
fn sink_card(line: &str) -> Option<i32> {
    line.strip_prefix("alsa.card = ")?
        .trim()
        .trim_matches('"')
        .parse()
        .ok()
}

/// Reads a port-list entry's port name from a line, if the line is one.
///
/// Port entries look like `analog-output-lineout: <description> (type: ...)`,
/// so the port name is the key before the first colon. Both the key and the
/// value are locale-independent, unlike the section header above them.
fn port_entry(line: &str) -> Option<&str> {
    let (key, _) = line.split_once(':')?;
    let key = key.trim_end();
    key.starts_with(ANALOG_PORT_PREFIX).then_some(key)
}

/// Reads the active port from a line, if the line is the active-port field.
///
/// The active-port field puts a bare port name *after* its (localized) label,
/// e.g. `Active Port: analog-output-headphones` or
/// `Puerto Activo: analog-output-headphones`. Matching on the unlocalized port
/// token instead of the label keeps the parser locale-independent.
fn active_port_entry(line: &str) -> Option<&str> {
    let (_, value) = line.split_once(':')?;
    let value = value.trim();
    (value.starts_with(ANALOG_PORT_PREFIX) && !value.contains(char::is_whitespace)).then_some(value)
}

/// Reads the sink volume percentage from a line, if the line carries one.
///
/// The label is localized (`Volume:` / `Volumen:`) but the channel names are
/// not, so the line is recognized by its channel names and the percentage is
/// read from the `<raw> / <percent>% / <decibels>` shape. The base-volume line
/// carries no channel names and is therefore ignored, as is the same shape
/// printed for a source or a stream.
fn volume_entry(line: &str) -> Option<u32> {
    if !VOLUME_CHANNEL_MARKERS
        .iter()
        .any(|marker| line.contains(marker))
    {
        return None;
    }

    line.split('/')
        .find_map(|part| part.trim().strip_suffix('%'))
        .and_then(|value| value.trim().parse().ok())
}

/// Is this the header line that opens a sink block?
///
/// `pactl` prints `Sink #12` in English and `Destino #12` in Spanish, so only
/// the index is locale-independent. It is what separates one sink's fields
/// from the next one's, and it matters because a localized output names the
/// sink only in its properties block — after the fields that come before it.
fn is_sink_header(line: &str) -> bool {
    let Some((label, index)) = line.rsplit_once('#') else {
        return false;
    };

    // A property value such as `device.id = "#12"` carries an equals sign and
    // a quote, and is not a block header.
    if label.contains('=') || label.contains('"') {
        return false;
    }

    !index.is_empty() && index.chars().all(|char| char.is_ascii_digit())
}

/// Parses `pactl list sinks` output into the sinks it describes.
///
/// Pure on purpose: the parsing rules (which fields are locale-independent,
/// where a sink starts and ends) are the part worth testing, and they must not
/// need a running sound server.
///
/// The output is split into sink blocks first, because a localized output
/// names the sink in its properties block: the fields that come before that
/// name — the volume among them — would otherwise land on the previous sink.
fn parse_sinks(output: &str) -> Vec<Sink> {
    let mut sinks: Vec<Sink> = Vec::new();
    let mut current = Sink::default();

    for raw in output.lines() {
        let line = raw.trim();

        if is_sink_header(line) {
            if !current.name.is_empty() {
                sinks.push(std::mem::take(&mut current));
            } else {
                current = Sink::default();
            }
            continue;
        }

        if let Some(name) = sink_name(line) {
            // `Name:` and `node.name` describe the same sink; the second one
            // must not replace the name the first one already gave.
            if current.name.is_empty() || current.name != name {
                current.name = name;
            }
            continue;
        }

        if let Some(port) = port_entry(line) {
            if !current.ports.iter().any(|existing| existing == port) {
                current.ports.push(port.to_owned());
            }
        } else if let Some(active) = active_port_entry(line) {
            current.active_port = Some(active.to_owned());
        } else if let Some(card) = sink_card(line) {
            current.alsa_card = Some(card);
        } else if let Some(volume) = volume_entry(line) {
            // The first volume line of a sink block is the live volume; the
            // base volume that follows it is rejected by the channel check.
            if current.volume.is_none() {
                current.volume = Some(volume);
            }
        }
    }

    if !current.name.is_empty() {
        sinks.push(current);
    }

    sinks
}

/// Picks the analog sink out of a parsed list.
///
/// When several sinks expose `analog-output-*` ports, the one named by
/// `pactl get-default-sink` wins; otherwise the first one is used.
fn find_analog_sink(output: &str, default_sink: Option<&str>) -> Option<Sink> {
    let mut sinks = parse_sinks(output);

    if let Some(default) = default_sink {
        if let Some(index) = sinks
            .iter()
            .position(|sink| sink.name == default && is_analog(sink))
        {
            return Some(sinks.swap_remove(index));
        }
    }

    let index = sinks.iter().position(is_analog)?;
    Some(sinks.swap_remove(index))
}

/// Discovers the analog sink, with everything the caller needs about it.
///
/// The sink is discovered instead of hardcoded because its name embeds the
/// machine's PCI address (`alsa_output.pci-0000_00_1f.3...`), which differs on
/// every host. A published applet cannot assume this developer's hardware. The
/// returned sink also carries the ALSA card it plays through, which is what
/// keeps `Auto-Mute Mode` aimed at the right codec on a multi-codec machine.
fn discover_analog_sink() -> Result<Sink, AudioError> {
    let output = pactl_stdout(&["list", "sinks"])?;
    let default = default_sink().ok().flatten();

    find_analog_sink(&output, default.as_deref()).ok_or(AudioError::NoAnalogSink)
}

/// Moves an already-discovered sink to `port`.
///
/// Split out so a caller that just discovered the sink does not pay for a
/// second `pactl list sinks`.
fn set_sink_port_on(sink: &str, port: &str) -> Result<(), AudioError> {
    let args = ["set-sink-port", sink, port];
    let output = pactl(&args)?;

    if output.status.success() {
        Ok(())
    } else {
        Err(AudioError::Command {
            args: args.join(" "),
            status: output.status,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// Writes a volume percentage onto an already-discovered sink.
///
/// The percentage is applied to the sink's *active* port, which is what makes
/// it the right call after a port switch: the value lands on the port the user
/// is now listening through instead of on the one they just left.
fn set_sink_volume_on(sink: &str, percent: u32) -> Result<(), AudioError> {
    let value = format!("{percent}%");
    let args = ["set-sink-volume", sink, value.as_str()];
    let output = pactl(&args)?;

    if output.status.success() {
        Ok(())
    } else {
        Err(AudioError::Command {
            args: args.join(" "),
            status: output.status,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// Live state of the analog sink: the active port and the current volume.
///
/// Both come from a single `pactl list sinks` call so the periodic check does
/// not pay for two processes every tick.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SinkState {
    /// Port the sink is playing through right now.
    pub port: Option<String>,
    /// Volume of that port, as a percentage of the base volume.
    pub volume: Option<u32>,
}

/// Reads the analog sink's active port and volume.
pub fn sink_state() -> Result<SinkState, AudioError> {
    let output = pactl_stdout(&["list", "sinks"])?;
    let default = default_sink().ok().flatten();

    Ok(find_analog_sink(&output, default.as_deref())
        .map(|sink| SinkState {
            port: sink.active_port,
            volume: sink.volume,
        })
        .unwrap_or_default())
}

/// Writes a volume percentage onto the analog sink's active port.
pub fn set_sink_volume(percent: u32) -> Result<(), AudioError> {
    let sink = discover_analog_sink()?;
    set_sink_volume_on(&sink.name, percent)
}

/// Chooses the enum item index whose label matches `wanted`.
///
/// The match is case-insensitive so a driver that reports `disabled` still
/// resolves. Index `0` is never assumed: if neither `Disabled` nor `Enabled` is
/// among the labels, the error names the labels actually seen so a mismatched
/// driver is diagnosable instead of silently ignored.
fn choose_auto_mute_item(items: &[String], wanted: &str) -> Result<u32, AudioError> {
    items
        .iter()
        .position(|item| item.eq_ignore_ascii_case(wanted))
        .map(|index| index as u32)
        .ok_or_else(|| AudioError::AutoMuteItemMissing {
            labels: items.to_vec(),
        })
}

/// Orders the mixer cards to try: the sink's own card first, then the rest.
///
/// A machine with a single analog codec has exactly one card owning
/// `Auto-Mute Mode`, so any order resolves. With two analog codecs, taking the
/// first match is a coin flip, so the card the discovered sink is backed by is
/// tried first and the remaining cards in enumeration order are kept as the
/// fallback for a sink whose property is missing (older sound servers, unusual
/// drivers) or stale (the card was renumbered after the sink list was read).
fn card_try_order(preferred: Option<i32>, cards: impl IntoIterator<Item = i32>) -> Vec<i32> {
    let mut order: Vec<i32> = cards.into_iter().collect();
    if let Some(preferred) = preferred {
        order.retain(|card| *card != preferred);
        order.insert(0, preferred);
    }
    order
}

/// Sets the HDA driver's `Auto-Mute Mode` control.
///
/// The mixer is opened per physical card (`hw:N`) rather than on `"default"`.
/// On a PipeWire/PulseAudio host the ALSA `"default"` control interface is the
/// sound-server plugin's reduced mixer (it exposes only `Master` and `Capture`),
/// so `Auto-Mute Mode` is simply not there — verified on this host with both
/// the `alsa` crate and `amixer -D default`. The `Auto-Mute Mode` control is a
/// kernel HDA codec control and only exists on the real card, so the codec is
/// found by scanning the cards for the one that owns the control.
///
/// When the caller knows which card the active sink plays through, that card is
/// tried first, so a second analog codec cannot win the scan by enumeration
/// order alone.
///
/// Closing (dropping) the mixer commits the change, so the `Mixer` must not be
/// leaked. The label matched against the enum items is the driver's, not a
/// localized string.
pub fn set_auto_mute(enabled: bool, sink_card: Option<i32>) -> Result<(), AudioError> {
    let wanted = if enabled { "Enabled" } else { "Disabled" };

    let cards = alsa::card::Iter::new()
        .map(|card| card.map_err(AudioError::Alsa).map(|card| card.get_index()))
        .collect::<Result<Vec<i32>, AudioError>>()?;

    for index in card_try_order(sink_card, cards) {
        let device = format!("hw:{index}");

        // A card whose control interface cannot be opened (or that does not
        // own `Auto-Mute Mode`, e.g. an HDMI-only codec) is simply skipped.
        let Ok(mixer) = Mixer::new(&device, false) else {
            continue;
        };
        let Some(selem) = mixer.find_selem(&SelemId::new(AUTO_MUTE_CONTROL, 0)) else {
            continue;
        };

        let items: Vec<String> = selem
            .iter_enum()
            .map_err(AudioError::Alsa)?
            .collect::<Result<Vec<String>, alsa::Error>>()
            .map_err(AudioError::Alsa)?;

        let index = choose_auto_mute_item(&items, wanted)?;

        // Writing the value the control already holds still notifies the
        // driver, and the driver reacts by re-routing the analog outputs. A
        // redundant write is therefore skipped, which keeps the periodic
        // guard free of side effects.
        if selem.get_enum_item(SelemChannelId::mono()).ok() != Some(index) {
            // `Auto-Mute Mode` is a single (mono) enum control, so any channel
            // works; `SelemChannelId::mono()` documents that intent.
            selem
                .set_enum_item(SelemChannelId::mono(), index)
                .map_err(AudioError::Alsa)?;
        }

        // `selem` (declared last) drops before `mixer`; closing the mixer is
        // what commits the change.
        return Ok(());
    }

    Err(AudioError::AutoMuteControlMissing)
}

/// Re-asserts the driver's `Auto-Mute Mode` on the analog sink's own card.
///
/// The driver can turn the control back on by itself after a jack event, and
/// a forced port does not survive that. This is the guard that puts it back,
/// aimed at the same card the sink plays through.
pub(crate) fn ensure_auto_mute(enabled: bool) -> Result<(), AudioError> {
    let sink = discover_analog_sink()?;
    set_auto_mute(enabled, sink.alsa_card)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Normal C-locale `pactl list sinks` output for this host's analog card.
    const C_LOCALE: &str = "\
Sink #57
\tState: RUNNING
\tName: alsa_output.pci-0000_00_1f.3.analog-stereo
\tDescription: Analog Stereo
\tVolume: front-left: 45875 /  70% / -9.29 dB,   front-right: 45875 /  70% / -9.29 dB
\tBase Volume: 65536 / 100% / 0.00 dB
\tProperties:
\t\talsa.card = \"0\"
\t\tapi.alsa.card = \"0\"
\t\tnode.name = \"alsa_output.pci-0000_00_1f.3.analog-stereo\"
\tPorts:
\t\tanalog-output-lineout: Line Out (type: Line, priority: 9000, availability group: Legacy 4, available)
\t\tanalog-output-headphones: Headphones (type: Headphones, priority: 9900, availability group: Legacy 5, available)
\tActive Port: analog-output-headphones
\tFormats:
\t\tpcm
";

    /// The same host under a Spanish locale: the section labels are translated
    /// (`Nombre:`, `Puertos:`, `Puerto Activo:`), which is exactly why the
    /// parser must anchor on locale-independent tokens and the caller forces
    /// `LC_ALL=C`.
    const SPANISH_LOCALE: &str = "\
Destino #57
\tEstado: RUNNING
\tNombre: alsa_output.pci-0000_00_1f.3.analog-stereo
\tDescripción: Audio Interno Estéreo analógico
\tVolumen: front-left: 45875 /  70% / -9,29 dB,   front-right: 45875 /  70% / -9,29 dB
\tVolumen base: 65536 / 100% / 0,00 dB
\tPropiedades:
\t\tnode.name = \"alsa_output.pci-0000_00_1f.3.analog-stereo\"
\tPuertos:
\t\tanalog-output-lineout: Línea de salida (tipo: Línea, prioridad: 9000, grupo de disponibilidad: Legacy 4, disponible)
\t\tanalog-output-headphones: Auriculares (tipo: Auriculares, prioridad: 9900, grupo de disponibilidad: Legacy 5, disponible)
\tPuerto Activo: analog-output-headphones
";

    /// A different machine: the analog sink's PCI address differs from this
    /// host's, so a hardcoded constant would fail here.
    const OTHER_PCI: &str = "\
Sink #9
\tState: SUSPENDED
\tName: alsa_output.pci-0000_00_1b.0.analog-stereo
\tProperties:
\t\tnode.name = \"alsa_output.pci-0000_00_1b.0.analog-stereo\"
\tPorts:
\t\tanalog-output-lineout: Line Out (type: Line, priority: 9000, availability group: Legacy 4, available)
\t\tanalog-output-headphones: Headphones (type: Headphones, priority: 9900, availability group: Legacy 5, available)
\tActive Port: analog-output-lineout
";

    /// Several sinks, only one of which exposes `analog-output-*` ports; the
    /// USB DAC has no ports and the HDMI sink only has `hdmi-output-*`.
    const MULTIPLE_SINKS: &str = "\
Sink #1
\tState: SUSPENDED
\tName: alsa_output.usb-Generic_USB_Audio-00.analog-stereo
\tProperties:
\t\tnode.name = \"alsa_output.usb-Generic_USB_Audio-00.analog-stereo\"
Sink #2
\tState: SUSPENDED
\tName: alsa_output.pci-0000_01_00.1.hdmi-stereo
\tProperties:
\t\tnode.name = \"alsa_output.pci-0000_01_00.1.hdmi-stereo\"
\tPorts:
\t\thdmi-output-0: HDMI / DisplayPort 1 (type: HDMI, priority: 5900, available)
Sink #3
\tState: RUNNING
\tName: alsa_output.pci-0000_00_1f.3.analog-stereo
\tProperties:
\t\tnode.name = \"alsa_output.pci-0000_00_1f.3.analog-stereo\"
\tPorts:
\t\tanalog-output-lineout: Line Out (type: Line, priority: 9000, available)
\t\tanalog-output-headphones: Headphones (type: Headphones, priority: 9900, available)
\tActive Port: analog-output-lineout
";

    /// Two sinks that both expose `analog-output-*` ports, used to check the
    /// `pactl get-default-sink` preference.
    const TWO_ANALOG_SINKS: &str = "\
Sink #1
\tState: SUSPENDED
\tName: alsa_output.usb-Generic_USB_Audio-00.analog-stereo
\tProperties:
\t\tnode.name = \"alsa_output.usb-Generic_USB_Audio-00.analog-stereo\"
\tPorts:
\t\tanalog-output-headphones: Headphones (type: Headphones, priority: 9900, available)
Sink #2
\tState: RUNNING
\tName: alsa_output.pci-0000_00_1f.3.analog-stereo
\tProperties:
\t\tnode.name = \"alsa_output.pci-0000_00_1f.3.analog-stereo\"
\tPorts:
\t\tanalog-output-lineout: Line Out (type: Line, priority: 9000, available)
\t\tanalog-output-headphones: Headphones (type: Headphones, priority: 9900, available)
\tActive Port: analog-output-lineout
";

    /// The decoy `api.alsa.*` properties PipeWire prints on the same sink.
    /// None of them is the sink's own card, so this fixture must yield no card
    /// at all: reading one of them would aim the mixer at another codec on
    /// machines where the two disagree.
    const API_ONLY_CARD: &str = "\
Sink #8
\tState: SUSPENDED
\tName: alsa_output.pci-0000_00_1f.3.analog-stereo
\tProperties:
\t\tapi.alsa.card = \"0\"
\t\tapi.alsa.pcm.card = \"5\"
\t\tnode.name = \"alsa_output.pci-0000_00_1f.3.analog-stereo\"
\tPorts:
\t\tanalog-output-lineout: Line Out (type: Line, priority: 9000, available)
";

    /// Two analog codecs on one machine, each able to own `Auto-Mute Mode`.
    /// Sink #2 is the one that is running and it is backed by card 1, so
    /// "first card that owns the control" would aim at the wrong codec.
    const MULTI_CODEC: &str = "\
Sink #1
\tState: SUSPENDED
\tName: alsa_output.pci-0000_00_1f.3.analog-stereo
\tProperties:
\t\talsa.card = \"0\"
\t\tnode.name = \"alsa_output.pci-0000_00_1f.3.analog-stereo\"
\tPorts:
\t\tanalog-output-lineout: Line Out (type: Line, priority: 9000, available)
Sink #2
\tState: RUNNING
\tName: alsa_output.pci-0000_00_1b.0.analog-stereo
\tProperties:
\t\talsa.card = \"1\"
\t\tnode.name = \"alsa_output.pci-0000_00_1b.0.analog-stereo\"
\tPorts:
\t\tanalog-output-lineout: Line Out (type: Line, priority: 9000, available)
\t\tanalog-output-headphones: Headphones (type: Headphones, priority: 9900, available)
\tActive Port: analog-output-lineout
";

    #[test]
    fn parses_c_locale_sink() {
        let sinks = parse_sinks(C_LOCALE);
        assert_eq!(sinks.len(), 1, "expected a single sink: {sinks:?}");
        assert_eq!(sinks[0].name, "alsa_output.pci-0000_00_1f.3.analog-stereo");
        assert_eq!(
            sinks[0].ports,
            vec!["analog-output-lineout", "analog-output-headphones"]
        );
        assert_eq!(
            sinks[0].active_port.as_deref(),
            Some("analog-output-headphones")
        );
    }

    #[test]
    fn reads_the_sink_volume() {
        // The volume is needed to carry the user's level across a port switch:
        // PipeWire keeps one volume per port.
        let sinks = parse_sinks(C_LOCALE);
        assert_eq!(sinks[0].volume, Some(70), "{sinks:?}");
    }

    #[test]
    fn reads_the_volume_under_a_translated_locale() {
        // The label is translated and the decimals use a comma, but the
        // channel names and the `<raw> / <percent>% / <db>` shape are not.
        let sink = find_analog_sink(SPANISH_LOCALE, None).expect("analog sink");
        assert_eq!(sink.volume, Some(70), "{sink:?}");
    }

    #[test]
    fn the_base_volume_is_not_the_sink_volume() {
        // The base volume follows the live one in the same block; taking it
        // would report a constant 100% and lose the user's level.
        assert_eq!(volume_entry("Base Volume: 65536 / 100% / 0.00 dB"), None);
        assert_eq!(volume_entry("Volumen base: 65536 / 100% / 0,00 dB"), None);
    }

    #[test]
    fn the_volume_is_read_from_the_percentage_segment() {
        assert_eq!(
            volume_entry("Volume: front-left: 45875 /  70% / -9.29 dB,   front-right: 45875 /  70% / -9.29 dB"),
            Some(70)
        );
        assert_eq!(
            volume_entry("Volumen: front-left: 32768 /  50% / -18,06 dB"),
            Some(50)
        );
        assert_eq!(
            volume_entry("Volume: mono: 65536 / 100% / 0.00 dB"),
            Some(100)
        );
    }

    #[test]
    fn a_sink_without_a_volume_line_reports_no_volume() {
        let sinks = parse_sinks(OTHER_PCI);
        assert_eq!(sinks[0].volume, None, "{sinks:?}");
    }

    #[test]
    fn the_sink_blocks_are_split_by_their_index() {
        // Each block carries its own fields. A localized output names the sink
        // only in its properties block, so without the split the second sink's
        // volume would land on the first one.
        let output = "\
Destino #1
\tVolumen: front-left: 45875 /  70% / -9,29 dB
\tnode.name = \"alsa_output.first.analog-stereo\"
Destino #2
\tVolumen: front-left: 26214 /  40% / -18,61 dB
\tnode.name = \"alsa_output.second.analog-stereo\"
";
        let sinks = parse_sinks(output);

        assert_eq!(sinks.len(), 2, "{sinks:?}");
        assert_eq!(sinks[0].name, "alsa_output.first.analog-stereo");
        assert_eq!(sinks[0].volume, Some(70), "{sinks:?}");
        assert_eq!(sinks[1].name, "alsa_output.second.analog-stereo");
        assert_eq!(sinks[1].volume, Some(40), "{sinks:?}");
    }

    #[test]
    fn a_property_value_is_not_a_block_header() {
        assert!(is_sink_header("Sink #57"));
        assert!(is_sink_header("Destino #57"));
        assert!(!is_sink_header("device.id = \"#12\""));
        assert!(!is_sink_header("Active Port: analog-output-lineout"));
        assert!(!is_sink_header(
            "Volume: front-left: 45875 / 70% / -9.29 dB"
        ));
    }

    #[test]
    fn parses_spanish_locale_sink() {
        // The Spanish labels must not matter: the sink and its active port are
        // recovered from the locale-independent tokens.
        let sink = find_analog_sink(SPANISH_LOCALE, None).expect("analog sink");
        assert_eq!(sink.name, "alsa_output.pci-0000_00_1f.3.analog-stereo");
        assert_eq!(
            sink.ports,
            vec!["analog-output-lineout", "analog-output-headphones"]
        );
        assert_eq!(
            sink.active_port.as_deref(),
            Some("analog-output-headphones")
        );
    }

    #[test]
    fn discovers_sink_on_different_pci_address() {
        // Regression test for the hardcoded `SINK` constant: the discovered
        // name must follow the machine, not this developer's address.
        let sink = find_analog_sink(OTHER_PCI, None).expect("analog sink");
        assert_eq!(sink.name, "alsa_output.pci-0000_00_1b.0.analog-stereo");
        assert_ne!(sink.name, "alsa_output.pci-0000_00_1f.3.analog-stereo");
        assert_eq!(sink.active_port.as_deref(), Some("analog-output-lineout"));
    }

    #[test]
    fn picks_only_the_analog_sink_among_many() {
        let sink = find_analog_sink(MULTIPLE_SINKS, None).expect("analog sink");
        assert_eq!(sink.name, "alsa_output.pci-0000_00_1f.3.analog-stereo");
        assert_eq!(sink.active_port.as_deref(), Some("analog-output-lineout"));
    }

    #[test]
    fn prefers_default_sink_when_several_are_analog() {
        // Both sinks are analog; the default must win on either side.
        let sink = find_analog_sink(
            TWO_ANALOG_SINKS,
            Some("alsa_output.pci-0000_00_1f.3.analog-stereo"),
        )
        .expect("analog sink");
        assert_eq!(sink.name, "alsa_output.pci-0000_00_1f.3.analog-stereo");

        let sink = find_analog_sink(
            TWO_ANALOG_SINKS,
            Some("alsa_output.usb-Generic_USB_Audio-00.analog-stereo"),
        )
        .expect("analog sink");
        assert_eq!(
            sink.name,
            "alsa_output.usb-Generic_USB_Audio-00.analog-stereo"
        );
    }

    fn items(labels: &[&str]) -> Vec<String> {
        labels.iter().map(|label| (*label).to_owned()).collect()
    }

    #[test]
    fn selects_disabled_and_enabled_case_insensitively() {
        let labels = items(&["Disabled", "Enabled"]);
        assert_eq!(choose_auto_mute_item(&labels, "Disabled").unwrap(), 0);
        assert_eq!(choose_auto_mute_item(&labels, "Enabled").unwrap(), 1);

        let lower = items(&["disabled", "enabled"]);
        assert_eq!(choose_auto_mute_item(&lower, "Disabled").unwrap(), 0);
        assert_eq!(choose_auto_mute_item(&lower, "Enabled").unwrap(), 1);
    }

    #[test]
    fn does_not_assume_index_zero() {
        let labels = items(&["Enabled", "Disabled"]);
        assert_eq!(choose_auto_mute_item(&labels, "Disabled").unwrap(), 1);
        assert_eq!(choose_auto_mute_item(&labels, "Enabled").unwrap(), 0);
    }

    #[test]
    fn reports_labels_when_neither_item_is_present() {
        let labels = items(&["Off", "On"]);
        let err = choose_auto_mute_item(&labels, "Disabled").expect_err("must fail");

        match &err {
            AudioError::AutoMuteItemMissing { labels } => {
                assert_eq!(labels, &items(&["Off", "On"]));
            }
            other => panic!("unexpected error: {other:?}"),
        }

        let message = err.to_string();
        assert!(message.contains("Off"), "{message}");
        assert!(message.contains("On"), "{message}");
    }

    #[test]
    fn maps_known_ports_to_their_spanish_labels() {
        assert_eq!(port_label(PORT_HEADPHONES), "Auriculares");
        assert_eq!(port_label(PORT_LINE_OUT), "Parlantes");
    }

    #[test]
    fn passes_unknown_ports_through_unchanged() {
        // The caption must never claim a state the hardware is not in, so an
        // unexpected port is shown verbatim instead of being mapped or hidden.
        assert_eq!(port_label("analog-output-hdmi"), "analog-output-hdmi");
    }

    #[test]
    fn cycles_through_every_mode_in_order() {
        assert_eq!(OutputMode::Speakers.next(), OutputMode::Headphones);
        assert_eq!(OutputMode::Headphones.next(), OutputMode::Auto);
        assert_eq!(OutputMode::Auto.next(), OutputMode::Speakers);
    }

    #[test]
    fn three_steps_return_to_the_starting_mode() {
        for start in [
            OutputMode::Speakers,
            OutputMode::Headphones,
            OutputMode::Auto,
        ] {
            assert_eq!(start.next().next().next(), start);
        }
    }

    #[test]
    fn tokens_round_trip_for_every_mode() {
        for mode in [
            OutputMode::Speakers,
            OutputMode::Headphones,
            OutputMode::Auto,
        ] {
            assert_eq!(
                OutputMode::from_token(mode.token()),
                Some(mode),
                "every mode must survive its own token"
            );
        }
    }

    #[test]
    fn tokens_are_not_the_spanish_labels() {
        // The persisted token must stay stable even if the UI copy is reworded.
        for mode in [
            OutputMode::Speakers,
            OutputMode::Headphones,
            OutputMode::Auto,
        ] {
            assert_ne!(mode.token(), mode.label());
        }
    }

    #[test]
    fn unknown_tokens_are_rejected() {
        assert_eq!(OutputMode::from_token(""), None);
        assert_eq!(OutputMode::from_token("Parlantes"), None);
        assert_eq!(OutputMode::from_token("speakers\n"), None);
    }

    #[test]
    fn parses_the_sinks_own_alsa_card() {
        let sinks = parse_sinks(C_LOCALE);
        assert_eq!(sinks.len(), 1, "expected a single sink: {sinks:?}");
        assert_eq!(sinks[0].alsa_card, Some(0), "{sinks:?}");
    }

    #[test]
    fn does_not_read_api_alsa_card_as_the_sink_card() {
        let sinks = parse_sinks(API_ONLY_CARD);
        assert_eq!(sinks.len(), 1, "expected a single sink: {sinks:?}");
        assert_eq!(
            sinks[0].alsa_card, None,
            "api.alsa.* is a different property: {sinks:?}"
        );
    }

    #[test]
    fn sinks_without_the_property_yield_no_card() {
        // The property is an optimization, not a requirement: a sound server
        // that does not print it must still parse, with the scan fallback left
        // in charge of finding the codec.
        let sinks = parse_sinks(TWO_ANALOG_SINKS);
        assert!(!sinks.is_empty(), "{sinks:?}");
        assert!(
            sinks.iter().all(|sink| sink.alsa_card.is_none()),
            "{sinks:?}"
        );
    }

    #[test]
    fn the_discovered_sink_carries_the_card_the_mixer_must_use() {
        // With two analog codecs, the sink that is actually playing has to
        // supply the card index, not the enumeration order.
        let sink = find_analog_sink(
            MULTI_CODEC,
            Some("alsa_output.pci-0000_00_1b.0.analog-stereo"),
        )
        .expect("analog sink");

        assert_eq!(sink.name, "alsa_output.pci-0000_00_1b.0.analog-stereo");
        assert_eq!(sink.alsa_card, Some(1));
    }

    #[test]
    fn the_sinks_card_is_tried_first_and_the_rest_stay_reachable() {
        assert_eq!(card_try_order(Some(1), [0, 1]), vec![1, 0]);
        assert_eq!(card_try_order(None, [0, 1]), vec![0, 1]);
        // A stale index is still tried first: opening `hw:7` fails harmlessly
        // and the scan continues, so a card renumbered behind our back cannot
        // make the applet give up.
        assert_eq!(card_try_order(Some(7), [0, 1]), vec![7, 0, 1]);
    }
}
