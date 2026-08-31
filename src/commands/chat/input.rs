use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event;

use crate::terminal::plain_text;

pub(super) use super::workbench::TriggerCandidate;
use super::workbench::{
    CandidateItem, CandidateView, Composer, ComposerAction, TranscriptKind, TranscriptText,
    WorkbenchEvent, WorkbenchModel, WorkbenchPhase, WorkbenchStatus, WorkbenchTerminal,
    style_editor,
};

const EDITOR_POLL_INTERVAL: Duration = Duration::from_millis(33);
const RESIZE_SETTLE_INTERVAL: Duration = Duration::from_millis(50);
const INTERRUPT_DEDUPE_INTERVAL: Duration = Duration::from_millis(100);
const DOUBLE_INTERRUPT_INTERVAL: Duration = Duration::from_millis(500);
const MAX_RUNTIME_EVENTS_PER_TICK: usize = 128;

pub(super) enum InteractiveRead {
    Line(String),
    Eof,
    Interrupted,
    TurnFinished,
    Failed(io::Error),
}

#[derive(Clone)]
pub(super) struct InteractivePrinter {
    target: OutputTarget,
}

#[derive(Clone)]
enum OutputTarget {
    Workbench(mpsc::Sender<WorkbenchEvent>),
    Plain,
}

impl InteractivePrinter {
    pub(super) fn print_line_as(
        &self,
        kind: TranscriptKind,
        line: impl Into<String>,
    ) -> io::Result<()> {
        let line = line.into();
        match &self.target {
            OutputTarget::Workbench(sender) => send_event(
                sender,
                WorkbenchEvent::OutputLine {
                    kind,
                    content: TranscriptText::plain_untrusted(&line),
                },
            ),
            OutputTarget::Plain => {
                println!("{}", plain_text(&line));
                Ok(())
            }
        }
    }

    pub(super) fn print_text_line_as(
        &self,
        kind: TranscriptKind,
        content: TranscriptText,
    ) -> io::Result<()> {
        match &self.target {
            OutputTarget::Workbench(sender) => {
                send_event(sender, WorkbenchEvent::OutputLine { kind, content })
            }
            OutputTarget::Plain => {
                println!("{}", plain_text(content.as_str()));
                Ok(())
            }
        }
    }

    #[cfg(test)]
    pub(super) fn write_fragment_as(&self, kind: TranscriptKind, fragment: &str) -> io::Result<()> {
        match &self.target {
            OutputTarget::Workbench(sender) => send_event(
                sender,
                WorkbenchEvent::OutputFragment {
                    kind,
                    content: TranscriptText::stream_untrusted(fragment, Default::default()),
                },
            ),
            OutputTarget::Plain => {
                print!("{fragment}");
                io::stdout().flush()
            }
        }
    }

    pub(super) fn write_text_fragment_as(
        &self,
        kind: TranscriptKind,
        content: TranscriptText,
    ) -> io::Result<()> {
        match &self.target {
            OutputTarget::Workbench(sender) => {
                send_event(sender, WorkbenchEvent::OutputFragment { kind, content })
            }
            OutputTarget::Plain => {
                print!("{}", plain_text(content.as_str()));
                io::stdout().flush()
            }
        }
    }

    pub(super) fn promote_reasoning(&self, content: TranscriptText) -> io::Result<()> {
        match &self.target {
            OutputTarget::Workbench(sender) => {
                send_event(sender, WorkbenchEvent::PromoteReasoning { content })
            }
            OutputTarget::Plain => Ok(()),
        }
    }

    pub(super) fn finish_line(&self) -> io::Result<()> {
        match &self.target {
            OutputTarget::Workbench(sender) => send_event(sender, WorkbenchEvent::FinishOutputLine),
            OutputTarget::Plain => Ok(()),
        }
    }

    pub(super) fn status_changed(&self, status: WorkbenchStatus) -> io::Result<()> {
        match &self.target {
            OutputTarget::Workbench(sender) => {
                send_event(sender, WorkbenchEvent::StatusChanged(status))
            }
            OutputTarget::Plain => Ok(()),
        }
    }

    pub(super) fn clear_transcript(&self) -> io::Result<()> {
        match &self.target {
            OutputTarget::Workbench(sender) => {
                send_event(sender, WorkbenchEvent::TranscriptCleared)
            }
            OutputTarget::Plain => Ok(()),
        }
    }

    pub(super) const fn is_workbench(&self) -> bool {
        matches!(self.target, OutputTarget::Workbench(_))
    }

    fn operation_started(&self) -> io::Result<()> {
        match &self.target {
            OutputTarget::Workbench(sender) => send_event(sender, WorkbenchEvent::OperationStarted),
            OutputTarget::Plain => Ok(()),
        }
    }

    fn operation_finished(&self) -> io::Result<()> {
        match &self.target {
            OutputTarget::Workbench(sender) => {
                send_event(sender, WorkbenchEvent::OperationFinished)
            }
            OutputTarget::Plain => Ok(()),
        }
    }

    #[cfg(test)]
    pub(super) fn test_workbench() -> (Self, mpsc::Receiver<WorkbenchEvent>) {
        let (sender, receiver) = mpsc::channel();
        (
            Self {
                target: OutputTarget::Workbench(sender),
            },
            receiver,
        )
    }
}

fn send_event(sender: &mpsc::Sender<WorkbenchEvent>, event: WorkbenchEvent) -> io::Result<()> {
    sender
        .send(event)
        .map_err(|_| io::Error::other("workbench event channel disconnected"))
}

#[derive(Clone)]
pub(super) struct TurnWake {
    turn_finished: Arc<AtomicBool>,
    active_reader: Arc<Mutex<Option<mpsc::Sender<InteractiveRead>>>>,
    printer: InteractivePrinter,
}

impl TurnWake {
    pub(super) fn finished(&self) {
        self.turn_finished.store(true, Ordering::Release);
        if self.printer.is_workbench() {
            let _ = self.printer.operation_finished();
            return;
        }
        if let Ok(reader) = self.active_reader.try_lock()
            && let Some(sender) = reader.as_ref()
        {
            let _ = sender.send(InteractiveRead::TurnFinished);
        }
    }
}

pub(super) struct InteractiveInput {
    interrupted: Arc<AtomicBool>,
    turn_finished: Arc<AtomicBool>,
    active_reader: Arc<Mutex<Option<mpsc::Sender<InteractiveRead>>>>,
    workbench: Option<Mutex<RichWorkbench>>,
    printer: InteractivePrinter,
    #[cfg(test)]
    scripted_reads: Option<Mutex<mpsc::Receiver<InteractiveRead>>>,
}

impl InteractiveInput {
    pub(super) fn install(
        rich: bool,
        color: bool,
        reference_candidates: Vec<TriggerCandidate>,
        initial_status: WorkbenchStatus,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let interrupted = Arc::new(AtomicBool::new(false));
        let turn_finished = Arc::new(AtomicBool::new(false));
        let active_reader = Arc::new(Mutex::new(None::<mpsc::Sender<InteractiveRead>>));
        let handler_interrupted = Arc::clone(&interrupted);
        let handler_reader = Arc::clone(&active_reader);
        if rich {
            ctrlc::set_handler(move || {
                signal_interactive_interrupt(&handler_interrupted, &handler_reader);
            })?;
        }

        let (printer, workbench) = if rich {
            let (sender, receiver) = mpsc::channel();
            let printer = InteractivePrinter {
                target: OutputTarget::Workbench(sender.clone()),
            };
            let workbench = RichWorkbench::new(
                color,
                reference_candidates,
                initial_status,
                sender,
                receiver,
            )?;
            (printer, Some(Mutex::new(workbench)))
        } else {
            (
                InteractivePrinter {
                    target: OutputTarget::Plain,
                },
                None,
            )
        };

        Ok(Self {
            interrupted,
            turn_finished,
            active_reader,
            workbench,
            printer,
            #[cfg(test)]
            scripted_reads: None,
        })
    }

    #[cfg(test)]
    pub(super) fn scripted() -> (Self, mpsc::Sender<InteractiveRead>) {
        let (sender, receiver) = mpsc::channel();
        let active_reader = Arc::new(Mutex::new(Some(sender.clone())));
        (
            Self {
                interrupted: Arc::new(AtomicBool::new(false)),
                turn_finished: Arc::new(AtomicBool::new(false)),
                active_reader,
                workbench: None,
                printer: InteractivePrinter {
                    target: OutputTarget::Plain,
                },
                scripted_reads: Some(Mutex::new(receiver)),
            },
            sender,
        )
    }

    pub(super) fn printer(&self) -> InteractivePrinter {
        self.printer.clone()
    }

    pub(super) fn turn_wake(&self) -> TurnWake {
        TurnWake {
            turn_finished: Arc::clone(&self.turn_finished),
            active_reader: Arc::clone(&self.active_reader),
            printer: self.printer.clone(),
        }
    }

    pub(super) fn read_message(&self) -> InteractiveRead {
        self.read(false)
    }

    pub(super) fn read_live_message(&self) -> InteractiveRead {
        self.read(true)
    }

    fn read(&self, live: bool) -> InteractiveRead {
        #[cfg(test)]
        if let Some(scripted_reads) = &self.scripted_reads {
            return match scripted_reads.lock() {
                Ok(receiver) => receiver.recv().unwrap_or(InteractiveRead::Eof),
                Err(_) => {
                    InteractiveRead::Failed(io::Error::other("scripted input state is unavailable"))
                }
            };
        }
        let Some(workbench) = &self.workbench else {
            return self.read_plain_line();
        };
        let mut workbench = match workbench.lock() {
            Ok(workbench) => workbench,
            Err(_) => {
                return InteractiveRead::Failed(io::Error::other(
                    "interactive workbench state is unavailable",
                ));
            }
        };
        match workbench.read(&self.interrupted, live) {
            Ok(read) => read,
            Err(error) => InteractiveRead::Failed(error),
        }
    }

    fn read_plain_line(&self) -> InteractiveRead {
        if self.take_interrupted() {
            return InteractiveRead::Interrupted;
        }
        if self.take_turn_finished() {
            return InteractiveRead::TurnFinished;
        }
        let (sender, receiver) = mpsc::channel();
        if let Ok(mut active) = self.active_reader.lock() {
            *active = Some(sender.clone());
        } else {
            return InteractiveRead::Failed(io::Error::other(
                "interactive input state is unavailable",
            ));
        }
        if self.take_interrupted() {
            self.clear_active_reader();
            return InteractiveRead::Interrupted;
        }
        if self.take_turn_finished() {
            self.clear_active_reader();
            return InteractiveRead::TurnFinished;
        }
        thread::spawn(move || {
            let mut line = String::new();
            let event = match io::stdin().read_line(&mut line) {
                Ok(0) => InteractiveRead::Eof,
                Ok(_) => InteractiveRead::Line(line),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                    InteractiveRead::Interrupted
                }
                Err(error) => InteractiveRead::Failed(error),
            };
            let _ = sender.send(event);
        });
        let event = receiver.recv().unwrap_or(InteractiveRead::Interrupted);
        self.clear_active_reader();
        if self.take_interrupted() {
            InteractiveRead::Interrupted
        } else {
            event
        }
    }

    pub(super) fn take_interrupted(&self) -> bool {
        self.interrupted.swap(false, Ordering::AcqRel)
    }

    fn take_turn_finished(&self) -> bool {
        self.turn_finished.swap(false, Ordering::AcqRel)
    }

    pub(super) fn begin_live_turn(&self) {
        self.turn_finished.store(false, Ordering::Release);
        let _ = self.printer.operation_started();
    }

    pub(super) fn finish_live_turn(&self) {
        self.turn_finished.store(false, Ordering::Release);
    }

    pub(super) fn close(&self) -> io::Result<()> {
        let Some(workbench) = &self.workbench else {
            return Ok(());
        };
        workbench
            .lock()
            .map_err(|_| io::Error::other("interactive workbench state is unavailable"))?
            .finish()
    }

    fn clear_active_reader(&self) {
        if let Ok(mut active) = self.active_reader.lock() {
            *active = None;
        }
    }
}

pub(super) fn signal_interactive_interrupt(
    interrupted: &AtomicBool,
    active_reader: &Mutex<Option<mpsc::Sender<InteractiveRead>>>,
) {
    interrupted.store(true, Ordering::Release);
    if let Ok(reader) = active_reader.try_lock()
        && let Some(sender) = reader.as_ref()
    {
        let _ = sender.send(InteractiveRead::Interrupted);
    }
}

struct RichWorkbench {
    terminal: Option<WorkbenchTerminal>,
    model: WorkbenchModel,
    composer: Composer,
    sender: mpsc::Sender<WorkbenchEvent>,
    events: mpsc::Receiver<WorkbenchEvent>,
    pending_input: VecDeque<event::Event>,
    next_submission_id: u64,
    interrupt_deduper: InterruptDeduper,
    idle_interrupt: IdleInterrupt,
}

impl RichWorkbench {
    fn new(
        color: bool,
        reference_candidates: Vec<TriggerCandidate>,
        initial_status: WorkbenchStatus,
        sender: mpsc::Sender<WorkbenchEvent>,
        events: mpsc::Receiver<WorkbenchEvent>,
    ) -> io::Result<Self> {
        // Terminal construction must happen before the first crossterm event read.
        let terminal = WorkbenchTerminal::new(color)?;
        let mut composer = Composer::new(reference_candidates);
        style_composer(&mut composer, color);
        Ok(Self {
            terminal: Some(terminal),
            model: WorkbenchModel::new(initial_status),
            composer,
            sender,
            events,
            pending_input: VecDeque::new(),
            next_submission_id: 1,
            interrupt_deduper: InterruptDeduper::default(),
            idle_interrupt: IdleInterrupt::default(),
        })
    }

    fn read(&mut self, interrupted: &AtomicBool, live: bool) -> io::Result<InteractiveRead> {
        self.draw()?;
        loop {
            if interrupted.swap(false, Ordering::AcqRel) {
                if let Some(read) = self.handle_interrupt_event(InterruptSource::Signal, live)? {
                    return Ok(read);
                }
                continue;
            }
            if self.model.take_operation_finished() {
                return Ok(InteractiveRead::TurnFinished);
            }
            // Input gets first chance on every tick. A fast stream can otherwise
            // keep the runtime queue permanently non-empty and make `/stop`,
            // steering, and normal editing appear frozen until the turn ends.
            if let Some(event) = self.next_composer_event(Duration::ZERO)?
                && let Some(read) = self.handle_composer_event(event, live)?
            {
                return Ok(read);
            }
            let drained = self.drain_runtime_events()?;
            if drained > 0 {
                self.draw()?;
            }
            if self.model.take_operation_finished() {
                return Ok(InteractiveRead::TurnFinished);
            }

            if let Some(event) = self.next_composer_event(EDITOR_POLL_INTERVAL)?
                && let Some(read) = self.handle_composer_event(event, live)?
            {
                return Ok(read);
            }
        }
    }

    fn drain_runtime_events(&mut self) -> io::Result<usize> {
        self.drain_events(MAX_RUNTIME_EVENTS_PER_TICK, None)
            .map(|result| result.drained)
    }

    fn drain_through_submission(&mut self, submission_id: u64) -> io::Result<()> {
        let result = self.drain_events(usize::MAX, Some(submission_id))?;
        if result.reached_submission {
            Ok(())
        } else {
            Err(io::Error::other(
                "submitted input was not received by the workbench",
            ))
        }
    }

    fn drain_events(
        &mut self,
        limit: usize,
        through_submission: Option<u64>,
    ) -> io::Result<DrainResult> {
        let mut drained = 0;
        let mut reached_submission = false;
        for _ in 0..limit {
            match self.events.try_recv() {
                Ok(event) => {
                    let clears_transcript = matches!(&event, WorkbenchEvent::TranscriptCleared);
                    reached_submission = matches!(
                        &event,
                        WorkbenchEvent::InputSubmitted { id, .. }
                            if Some(*id) == through_submission
                    );
                    drained += 1;
                    self.model.apply(event);
                    if clears_transcript {
                        self.terminal_mut()?.reset_transcript();
                    }
                    if reached_submission {
                        break;
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err(io::Error::other("workbench event channel disconnected"));
                }
            }
        }
        Ok(DrainResult {
            drained,
            reached_submission,
        })
    }

    fn handle_composer_event(
        &mut self,
        event: event::Event,
        live: bool,
    ) -> io::Result<Option<InteractiveRead>> {
        if let event::Event::Resize(width, height) = event {
            let (width, height) = self.coalesce_resize(width, height)?;
            self.terminal_mut()?.resize(width.max(1), height.max(1))?;
            self.draw()?;
            return Ok(None);
        }
        if self.handle_viewport_event(&event) {
            self.draw()?;
            return Ok(None);
        }
        match self.composer.handle_event(event) {
            ComposerAction::Submit(line) => {
                let display = submitted_lines(&line);
                let submission_id = self.next_submission_id;
                self.next_submission_id = self.next_submission_id.wrapping_add(1);
                send_event(
                    &self.sender,
                    WorkbenchEvent::InputSubmitted {
                        id: submission_id,
                        lines: display,
                    },
                )?;
                self.drain_through_submission(submission_id)?;
                self.draw()?;
                Ok(Some(InteractiveRead::Line(line)))
            }
            ComposerAction::Interrupt => {
                self.handle_interrupt_event(InterruptSource::Crossterm, live)
            }
            ComposerAction::Eof => Ok(Some(InteractiveRead::Eof)),
            ComposerAction::Redraw => {
                self.draw()?;
                Ok(None)
            }
            ComposerAction::None => Ok(None),
        }
    }

    fn handle_viewport_event(&mut self, input: &event::Event) -> bool {
        let Some(terminal) = self.terminal.as_mut() else {
            return false;
        };
        match input {
            event::Event::Key(key)
                if key.kind != event::KeyEventKind::Release && key.modifiers.is_empty() =>
            {
                match key.code {
                    event::KeyCode::PageUp => {
                        terminal.page_up();
                        true
                    }
                    event::KeyCode::PageDown => {
                        terminal.page_down();
                        true
                    }
                    _ => false,
                }
            }
            _ => false,
        }
    }

    fn handle_interrupt_event(
        &mut self,
        source: InterruptSource,
        live: bool,
    ) -> io::Result<Option<InteractiveRead>> {
        if self.interrupt_deduper.is_duplicate(source, Instant::now()) {
            return Ok(None);
        }
        self.handle_interrupt(live)
    }

    fn handle_interrupt(&mut self, live: bool) -> io::Result<Option<InteractiveRead>> {
        if live {
            return Ok(Some(InteractiveRead::Interrupted));
        }
        let exit = self.idle_interrupt.register(Instant::now());
        self.composer.clear();
        self.draw()?;
        Ok(exit.then_some(InteractiveRead::Eof))
    }

    fn next_composer_event(&mut self, timeout: Duration) -> io::Result<Option<event::Event>> {
        if let Some(event) = self.pending_input.pop_front() {
            return Ok(Some(event));
        }
        if event::poll(timeout)? {
            event::read().map(Some)
        } else {
            Ok(None)
        }
    }

    fn coalesce_resize(&mut self, width: u16, height: u16) -> io::Result<(u16, u16)> {
        let mut latest = (width, height);
        let mut quiet_until = Instant::now() + RESIZE_SETTLE_INTERVAL;
        loop {
            let remaining = quiet_until.saturating_duration_since(Instant::now());
            if remaining.is_zero() || !event::poll(remaining)? {
                break;
            }
            match event::read()? {
                event::Event::Resize(width, height) => {
                    latest = (width, height);
                    quiet_until = Instant::now() + RESIZE_SETTLE_INTERVAL;
                }
                input => self.pending_input.push_back(input),
            }
        }
        Ok(latest)
    }

    fn draw(&mut self) -> io::Result<()> {
        let (candidate_values, selected) = self.composer.completion().map_or_else(
            || (Vec::new(), 0),
            |completion| {
                (
                    completion
                        .items()
                        .iter()
                        .map(|item| (item.value.clone(), item.description.clone()))
                        .collect::<Vec<_>>(),
                    completion.selected(),
                )
            },
        );
        let candidates = candidate_values
            .iter()
            .map(|(value, description)| CandidateItem::new(value, Some(description)))
            .collect::<Vec<_>>();
        let candidate_view = if candidates.is_empty() {
            CandidateView::empty()
        } else {
            CandidateView::new(&candidates, selected)
        };

        let Self {
            terminal,
            model,
            composer,
            ..
        } = self;
        terminal
            .as_mut()
            .ok_or_else(|| io::Error::other("interactive workbench is closed"))?
            .draw(
                &model.status,
                model.transcript(),
                model.live_output(),
                model.live_kind(),
                composer.textarea_mut(),
                candidate_view,
            )
    }

    fn finish(&mut self) -> io::Result<()> {
        loop {
            let drained = self.drain_runtime_events()?;
            if drained < MAX_RUNTIME_EVENTS_PER_TICK {
                break;
            }
        }
        self.terminal
            .take()
            .map_or(Ok(()), WorkbenchTerminal::finish)
    }

    fn terminal_mut(&mut self) -> io::Result<&mut WorkbenchTerminal> {
        self.terminal
            .as_mut()
            .ok_or_else(|| io::Error::other("interactive workbench is closed"))
    }
}

struct DrainResult {
    drained: usize,
    reached_submission: bool,
}

fn style_composer(composer: &mut Composer, color: bool) {
    style_editor(
        composer.textarea_mut(),
        color,
        WorkbenchPhase::Ready.composer_placeholder(),
    );
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InterruptSource {
    Crossterm,
    Signal,
}

impl InterruptSource {
    const fn counterpart(self) -> Self {
        match self {
            Self::Crossterm => Self::Signal,
            Self::Signal => Self::Crossterm,
        }
    }
}

#[derive(Default)]
struct InterruptDeduper {
    expected_counterpart: Option<(InterruptSource, Instant)>,
}

impl InterruptDeduper {
    fn is_duplicate(&mut self, source: InterruptSource, now: Instant) -> bool {
        if self
            .expected_counterpart
            .is_some_and(|(expected, deadline)| source == expected && now <= deadline)
        {
            self.expected_counterpart = None;
            return true;
        }

        self.expected_counterpart = Some((source.counterpart(), now + INTERRUPT_DEDUPE_INTERVAL));
        false
    }
}

#[derive(Default)]
struct IdleInterrupt {
    last: Option<Instant>,
}

impl IdleInterrupt {
    fn register(&mut self, now: Instant) -> bool {
        let repeated = self
            .last
            .is_some_and(|last| now.saturating_duration_since(last) <= DOUBLE_INTERRUPT_INTERVAL);
        self.last = Some(now);
        repeated
    }
}

pub(super) fn submitted_lines(message: &str) -> Vec<String> {
    if message.trim().is_empty() {
        return Vec::new();
    }
    plain_text(message)
        .split('\n')
        .enumerate()
        .map(|(index, line)| {
            if index == 0 {
                format!("❯ {line}")
            } else {
                format!("  {line}")
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordered_workbench_events_do_not_turn_long_fragments_into_fake_lines() {
        let (printer, events) = InteractivePrinter::test_workbench();
        let fragment = "界".repeat(128);
        printer
            .write_fragment_as(TranscriptKind::Reasoning, &fragment)
            .unwrap();
        printer.finish_line().unwrap();

        match events.recv().unwrap() {
            WorkbenchEvent::OutputFragment { kind, content } => {
                assert_eq!(kind, TranscriptKind::Reasoning);
                assert_eq!(content.as_str(), fragment);
            }
            _ => panic!("expected output fragment"),
        }
        assert!(matches!(
            events.recv().unwrap(),
            WorkbenchEvent::FinishOutputLine
        ));
    }

    #[test]
    fn turn_finished_follows_all_output_on_the_same_queue() {
        let (printer, events) = InteractivePrinter::test_workbench();
        let wake = TurnWake {
            turn_finished: Arc::new(AtomicBool::new(false)),
            active_reader: Arc::new(Mutex::new(None)),
            printer: printer.clone(),
        };
        printer
            .print_line_as(TranscriptKind::Assistant, "last")
            .unwrap();
        wake.finished();

        assert!(matches!(
            events.recv().unwrap(),
            WorkbenchEvent::OutputLine { kind: TranscriptKind::Assistant, content }
                if content.as_str() == "last"
        ));
        assert!(matches!(
            events.recv().unwrap(),
            WorkbenchEvent::OperationFinished
        ));
    }

    #[test]
    fn interrupt_wakes_plain_reader_without_affecting_turn_completion() {
        let interrupted = AtomicBool::new(false);
        let (sender, receiver) = mpsc::channel();
        let active_reader = Mutex::new(Some(sender));

        signal_interactive_interrupt(&interrupted, &active_reader);
        assert!(interrupted.load(Ordering::Acquire));
        assert!(matches!(
            receiver.recv().unwrap(),
            InteractiveRead::Interrupted
        ));
    }

    #[test]
    fn submitted_multiline_text_is_compact_and_terminal_safe() {
        assert_eq!(
            submitted_lines("第一行🙂\nsecond\u{1b}"),
            ["❯ 第一行🙂", "  second\\u{1b}"]
        );
        assert!(submitted_lines("  \n").is_empty());
    }

    #[test]
    fn idle_ctrl_c_needs_a_second_press_within_half_a_second() {
        let start = Instant::now();
        let mut interrupt = IdleInterrupt::default();
        assert!(!interrupt.register(start));
        assert!(!interrupt.register(start + DOUBLE_INTERRUPT_INTERVAL + Duration::from_millis(1)));
        assert!(interrupt.register(start + DOUBLE_INTERRUPT_INTERVAL + Duration::from_millis(200)));
    }

    #[test]
    fn crossterm_then_signal_counts_as_one_idle_interrupt() {
        let start = Instant::now();
        let mut deduper = InterruptDeduper::default();
        let mut idle = IdleInterrupt::default();

        assert!(!register_idle_interrupt(
            &mut deduper,
            &mut idle,
            InterruptSource::Crossterm,
            start,
        ));
        assert!(!register_idle_interrupt(
            &mut deduper,
            &mut idle,
            InterruptSource::Signal,
            start + Duration::from_millis(20),
        ));
        assert!(register_idle_interrupt(
            &mut deduper,
            &mut idle,
            InterruptSource::Crossterm,
            start + Duration::from_millis(200),
        ));
    }

    #[test]
    fn signal_then_crossterm_counts_as_one_idle_interrupt() {
        let start = Instant::now();
        let mut deduper = InterruptDeduper::default();
        let mut idle = IdleInterrupt::default();

        assert!(!register_idle_interrupt(
            &mut deduper,
            &mut idle,
            InterruptSource::Signal,
            start,
        ));
        assert!(!register_idle_interrupt(
            &mut deduper,
            &mut idle,
            InterruptSource::Crossterm,
            start + Duration::from_millis(20),
        ));
        assert!(register_idle_interrupt(
            &mut deduper,
            &mut idle,
            InterruptSource::Signal,
            start + Duration::from_millis(200),
        ));
    }

    #[test]
    fn counterpart_after_dedupe_window_is_an_independent_interrupt() {
        let start = Instant::now();
        let mut deduper = InterruptDeduper::default();

        assert!(!deduper.is_duplicate(InterruptSource::Crossterm, start));
        assert!(!deduper.is_duplicate(
            InterruptSource::Signal,
            start + INTERRUPT_DEDUPE_INTERVAL + Duration::from_millis(1),
        ));
    }

    fn register_idle_interrupt(
        deduper: &mut InterruptDeduper,
        idle: &mut IdleInterrupt,
        source: InterruptSource,
        now: Instant,
    ) -> bool {
        !deduper.is_duplicate(source, now) && idle.register(now)
    }
}
