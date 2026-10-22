//! Event-driven watcher for changes on a sink.
//!
//! The applet has to notice that the driver moved the analog sink's port, and
//! it has to notice it *fast*. The COSMIC panel's own volume control resets
//! the sink's route through `com.system76.CosmicSettings.Audio.SetSinkVolume`,
//! and PipeWire picks the other analog port a couple of milliseconds later. A
//! poll can only ever see that flip one poll interval late, so the applet also
//! listens to the sound server's event stream.
//!
//! `pactl subscribe` is a long-lived child process that prints one line per
//! event, e.g. `Event 'change' on sink #56`. Only the *sink* events matter
//! here: an event about a client, a playback stream, a source, a card, a
//! module or the server cannot move the analog sink's port.
//!
//! Two properties of that output are easy to get wrong:
//!
//! - The event and object names are **translated**. The Spanish locale of this
//!   host prints `Evento 'cambio' en destino #56`, so the child is started
//!   with `LC_ALL=C` and the parser only understands the C locale.
//! - The line carries no properties, only the fact that a sink changed. The
//!   port still has to be read from the sink list.

use std::io::{BufRead, BufReader};
use std::pin::Pin;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::task::{Context, Poll};
use std::time::Duration;

use cosmic::iced::Subscription;
use futures::channel::mpsc::UnboundedSender;
use futures::Stream;

/// A change reported on a sink.
///
/// The event carries no payload on purpose: the line says *that* a sink
/// changed, not what changed, so the port has to be read from the sink list
/// anyway. A payload would only duplicate it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SinkEvent;

/// Identity of the watcher subscription.
///
/// The recipe's identity is what keeps the subscription alive between frames:
/// as long as this value does not change, the runtime reuses the running
/// stream instead of starting a second `pactl subscribe`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Watcher;

/// Pause before the subscriber is started again after it stopped on its own.
///
/// A subscriber that died (the sound server restarted, the process was
/// killed) is worth another try, but not in a tight loop.
const RETRY: Duration = Duration::from_secs(5);

/// Prefix every event line starts with.
const EVENT_PREFIX: &str = "Event '";

/// The object a sink event is about.
const SINK_OBJECT: &str = "sink";

/// The program that reports the sound server's events.
const SUBSCRIBER: &str = "pactl";

/// The subscription that reports the sink events.
pub fn subscription() -> Subscription<SinkEvent> {
    Subscription::run_with(Watcher, |_| events())
}

/// The stream of sink events.
///
/// The stream owns the watcher task: dropping it (when the applet goes away)
/// aborts the task, which drops its future, which drops the [`Subscriber`],
/// which kills the child instead of leaving it behind.
fn events() -> EventStream {
    let (sender, receiver) = futures::channel::mpsc::unbounded::<SinkEvent>();

    let task = tokio::task::spawn(async move {
        watch(sender).await;
    });

    EventStream { receiver, task }
}

/// The sink events and the task that produces them.
///
/// The task is what keeps the child process from outliving the applet: it
/// owns the [`Subscriber`], and this stream owns the task, so dropping the
/// stream aborts the task and the subscriber's `Drop` kills the child.
struct EventStream {
    /// The events the watcher reports.
    receiver: futures::channel::mpsc::UnboundedReceiver<SinkEvent>,
    /// The task that watches the subscriber.
    task: tokio::task::JoinHandle<()>,
}

impl Stream for EventStream {
    type Item = SinkEvent;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        Pin::new(&mut this.receiver).poll_next(cx)
    }
}

impl Drop for EventStream {
    fn drop(&mut self) {
        // Aborting the task drops its future, which drops the subscriber,
        // which kills the child: nobody is listening for the events any more.
        self.task.abort();
    }
}

/// Keeps a subscriber alive and forwards the sink events it reports.
///
/// The stream this task feeds must never end: iced keeps a subscription by
/// its identity and does not restart one whose stream has finished, so an
/// ended stream would leave the applet polling for the rest of its life.
async fn watch(sender: UnboundedSender<SinkEvent>) {
    let mut reported_failure = false;

    loop {
        // Nobody is listening for the events any more: the applet is gone,
        // so there is nothing to subscribe again for.
        if sender.is_closed() {
            return;
        }

        match Subscriber::spawn(SUBSCRIBER, &["subscribe"]) {
            Ok(mut subscriber) => {
                reported_failure = false;

                match subscriber.take_stdout() {
                    Some(stdout) => {
                        // The line reading runs on the blocking pool: it is a
                        // blocking read on a pipe, and it must not occupy the
                        // runtime's worker thread.
                        let events = sender.clone();
                        let reader = tokio::task::spawn_blocking(move || {
                            read_events(BufReader::new(stdout), events)
                        });

                        // The subscriber only stops when it is killed or the
                        // sound server goes away. Either way it is worth
                        // another try.
                        let _ = reader.await;
                    }
                    None => {
                        tracing::warn!("`pactl subscribe` produced no output pipe");
                    }
                }

                // The subscriber is dropped here, which kills it.
                drop(subscriber);

                tracing::debug!("`pactl subscribe` stopped; starting it again");
            }
            Err(error) => {
                // The subscriber could not be started at all. The applet keeps
                // polling in the meantime, and the subscriber is tried again:
                // the sound server may still be coming up.
                if !reported_failure {
                    tracing::warn!(%error, "could not start `pactl subscribe`; the applet keeps polling");
                    reported_failure = true;
                }
            }
        }

        tokio::time::sleep(RETRY).await;
    }
}

/// A long-lived `pactl subscribe` child process.
///
/// Dropping a [`std::process::Child`] leaves the process running, so the
/// subscriber is killed explicitly. Two things depend on it: a leftover
/// subscriber would keep a sound-server client open long after the applet is
/// gone, and the blocking read on its pipe would never end, which is what the
/// runtime's shutdown waits for.
struct Subscriber {
    child: Child,
    stdout: Option<ChildStdout>,
}

impl Subscriber {
    /// Starts the subscriber process.
    ///
    /// The program and its arguments are parameters so the plumbing can be
    /// exercised without a sound server.
    fn spawn(program: &str, args: &[&str]) -> std::io::Result<Self> {
        let mut child = Command::new(program)
            .args(args)
            // The event and object names are translated, so the C locale is
            // what makes the output parseable.
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;

        Ok(Self {
            stdout: child.stdout.take(),
            child,
        })
    }

    /// Takes the child's output pipe.
    ///
    /// The pipe moves to the blocking pool, but the child stays behind: it
    /// has to be killed when the task that owns it is dropped, or the
    /// blocking read would never end.
    fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.stdout.take()
    }
}

impl Drop for Subscriber {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Reads the subscriber's output and reports the sink events.
///
/// The lines are read as raw bytes and decoded leniently: a stray byte must
/// not end the subscription, and the event lines themselves are plain ASCII.
fn read_events<R: BufRead>(mut reader: R, sender: UnboundedSender<SinkEvent>) {
    let mut line = Vec::new();

    loop {
        line.clear();

        match reader.read_until(b'\n', &mut line) {
            // The subscriber ended, or the pipe broke.
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }

        let line = String::from_utf8_lossy(&line);

        if parse_event(&line).is_some() && sender.unbounded_send(SinkEvent).is_err() {
            // The applet is gone: nobody is listening any more.
            break;
        }
    }
}

/// Parses one line of the subscriber's output.
///
/// Returns `Some` for a line about a sink and `None` for anything else.
fn parse_event(line: &str) -> Option<SinkEvent> {
    // `Event '<name>' on <object> #<index>`
    let rest = line.trim().strip_prefix(EVENT_PREFIX)?;
    let (_name, rest) = rest.split_once('\'')?;
    let rest = rest.trim_start().strip_prefix("on ")?;
    let (object, index) = rest.split_once('#')?;

    // The index has to be there and has to be a number: a line that only
    // looks like an event is not one.
    let index = index.trim();
    if index.is_empty() || !index.chars().all(|char| char.is_ascii_digit()) {
        return None;
    }

    // The object is compared exactly: `sink-input` is a playback stream and
    // must not be mistaken for the sink itself.
    (object.trim() == SINK_OBJECT).then_some(SinkEvent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_sink_change() {
        assert_eq!(parse_event("Event 'change' on sink #56"), Some(SinkEvent));
    }

    #[test]
    fn reads_a_sink_creation_and_removal() {
        assert_eq!(parse_event("Event 'new' on sink #56"), Some(SinkEvent));
        assert_eq!(parse_event("Event 'remove' on sink #56"), Some(SinkEvent));
    }

    #[test]
    fn ignores_the_other_objects() {
        // None of these can move the analog sink's port.
        for line in [
            "Event 'new' on client #12",
            "Event 'remove' on client #12",
            "Event 'change' on source #3",
            "Event 'change' on card #0",
            "Event 'new' on module #7",
            "Event 'change' on server #0",
            "Event 'new' on sample-cache #4",
        ] {
            assert_eq!(parse_event(line), None, "{line} is not a sink event");
        }
    }

    #[test]
    fn ignores_a_playback_stream() {
        // `sink-input` is a stream, not the sink. Matching it would make
        // every stream start and stop trigger a correction.
        assert_eq!(parse_event("Event 'new' on sink-input #9"), None);
        assert_eq!(parse_event("Event 'new' on source-output #9"), None);
    }

    #[test]
    fn ignores_lines_that_are_not_events() {
        for line in [
            "",
            "Got SIGINT, exiting.",
            "Event 'change' on sink",
            "Event 'change' on sink #",
            "sink #56",
            "Event change on sink #56",
        ] {
            assert_eq!(parse_event(line), None, "{line} is not a sink event");
        }
    }

    #[test]
    fn ignores_a_translated_line() {
        // The subscriber's output is translated, which is why the child runs
        // with `LC_ALL=C`. A translated line must not be mistaken for an
        // event of an unknown object.
        assert_eq!(parse_event("Evento 'cambio' en destino #56"), None);
    }

    #[test]
    fn tolerates_surrounding_whitespace() {
        assert_eq!(
            parse_event("  Event 'change' on sink #56  "),
            Some(SinkEvent)
        );
    }

    /// Reads a byte stream and returns the events it reported.
    fn read(input: &[u8]) -> Vec<SinkEvent> {
        let (sender, mut receiver) = futures::channel::mpsc::unbounded();
        read_events(std::io::Cursor::new(input), sender);

        let mut events = Vec::new();
        while let Ok(event) = receiver.try_recv() {
            events.push(event);
        }
        events
    }

    #[test]
    fn reads_the_events_of_a_stream() {
        let events = read(
            b"Event 'change' on sink #56\n\
              Event 'new' on client #7\n\
              Event 'remove' on sink #56\n",
        );

        assert_eq!(events, vec![SinkEvent, SinkEvent]);
    }

    #[test]
    fn a_stray_byte_does_not_end_the_stream() {
        // The output is decoded leniently: a byte that is not valid UTF-8
        // must not cost the applet the events that follow it.
        let mut input = Vec::new();
        input.extend_from_slice(b"Event 'change' on sink #56\n");
        input.push(0xff);
        input.push(b'\n');
        input.extend_from_slice(b"Event 'change' on sink #57\n");

        assert_eq!(read(&input), vec![SinkEvent, SinkEvent]);
    }

    #[test]
    fn a_last_line_without_a_newline_is_still_read() {
        assert_eq!(
            read(b"Event 'change' on sink #56"),
            vec![SinkEvent],
            "the subscriber may end without a trailing newline"
        );
    }

    /// A stand-in for `pactl subscribe`.
    ///
    /// The real subscriber is a long-lived process; the stand-in prints what
    /// the real one prints and exits, which is enough to exercise the pipe,
    /// the decoding and the parser without a sound server.
    ///
    /// The stand-in is the shell itself and not a script written to disk: a
    /// file that was just written is still open for writing while the child
    /// is forked, and the kernel refuses to execute a file that is open for
    /// writing (`ETXTBSY`). Writing a script and running it right away is
    /// therefore a flaky way to test this.
    fn fake_subscriber(body: &'static str) -> (&'static str, [&'static str; 2]) {
        ("/bin/sh", ["-c", body])
    }

    #[test]
    fn reads_the_events_of_a_subscriber() {
        let (program, args) = fake_subscriber("echo \"Event 'change' on sink #56\"");

        let mut subscriber = Subscriber::spawn(program, &args).expect("the subscriber must start");
        let stdout = subscriber
            .take_stdout()
            .expect("the subscriber must have a pipe");

        let (sender, mut receiver) = futures::channel::mpsc::unbounded();
        read_events(BufReader::new(stdout), sender);

        assert_eq!(receiver.try_recv(), Ok(SinkEvent));
    }

    #[test]
    fn a_subscriber_that_prints_nothing_ends_the_stream() {
        // A subscriber with nothing to say must not keep the reader waiting:
        // the pipe closes when the process ends.
        let (program, args) = fake_subscriber("exit 0");

        let mut subscriber = Subscriber::spawn(program, &args).expect("the subscriber must start");
        let stdout = subscriber
            .take_stdout()
            .expect("the subscriber must have a pipe");

        let (sender, mut receiver) = futures::channel::mpsc::unbounded();
        read_events(BufReader::new(stdout), sender);

        assert!(receiver.try_recv().is_err(), "no event was reported");
    }

    #[test]
    fn dropping_the_subscriber_kills_it() {
        // The child has to die with the subscriber. A leftover subscriber would
        // keep a sound-server client open long after the applet is gone, and
        // the blocking read on its pipe would never end — which is exactly
        // what the runtime's shutdown waits for.
        //
        // `exec` matters here: without it the shell would leave a child
        // behind that holds the pipe open, and the reader would wait for that
        // one instead of for the subscriber.
        let (program, args) = fake_subscriber("exec sleep 2");

        let mut subscriber = Subscriber::spawn(program, &args).expect("the subscriber must start");
        let stdout = subscriber
            .take_stdout()
            .expect("the subscriber must have a pipe");

        let (sender, mut receiver) = futures::channel::mpsc::unbounded();
        drop(subscriber);

        // The pipe closes with the child, so the reader ends instead of
        // waiting for a process that is not there any more.
        read_events(BufReader::new(stdout), sender);

        assert!(receiver.try_recv().is_err(), "no event was reported");
    }

    #[test]
    fn dropping_the_stream_kills_the_process_its_task_owns() {
        // The promise the module makes: the child dies with the stream. The
        // task is what owns the subscriber, so dropping the stream has to
        // abort the task — a detached task would leave the child behind.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("the runtime must start");

        let (program, args) = fake_subscriber("exec sleep 5");

        let mut subscriber = Subscriber::spawn(program, &args).expect("the subscriber must start");
        let stdout = subscriber
            .take_stdout()
            .expect("the subscriber must have a pipe");

        runtime.block_on(async {
            let (_sender, receiver) = futures::channel::mpsc::unbounded();

            let task = tokio::task::spawn(async move {
                // The subscriber lives as long as the task: aborting the task
                // drops it, and its `Drop` kills the child.
                let _subscriber = subscriber;

                std::future::pending::<()>().await;
            });

            let stream = EventStream { receiver, task };

            drop(stream);

            // Give the runtime the chance to cancel the task and drop the
            // future that owns the child.
            tokio::time::sleep(Duration::from_millis(50)).await;
        });

        // The child is gone, so the pipe closed: the read ends instead of
        // waiting for a process that is not there any more.
        let started = std::time::Instant::now();
        let (sender, mut receiver) = futures::channel::mpsc::unbounded();
        read_events(BufReader::new(stdout), sender);

        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the child must die with the task that owns it"
        );
        assert!(receiver.try_recv().is_err(), "no event was reported");
    }
}
