//! Ratatui-based TUI. Entry point is [`Tui::start`].
use std::{
    collections::{HashMap, HashSet},
    fmt::Display,
    io::Write,
    str::FromStr,
    time::Duration,
};

use chrono::Utc;
use crossbeam_channel::Receiver;
use ratatui::{
    Frame, Terminal,
    backend::TerminaBackend,
    layout::{Constraint, Direction, Layout, Position, Rect},
    style::{Color, Style},
    termina::{
        Event as TerminaEvent, EventReader as TerminaEventReader, PlatformTerminal, Terminal as _,
        escape::csi::{Csi, DecPrivateMode, DecPrivateModeCode, Mode},
        event::{KeyCode, KeyEventKind, Modifiers as KeyModifiers},
    },
    text::{Line, Span},
    widgets::{Block, List, ListItem, Paragraph},
};

use crate::{
    event::Event,
    matcher::{FuzzyEngine, FuzzyIndex, Match},
};

/// Non-blocking drain of the [`Event`] channel fed by the caller of [`Tui::start`].
struct EventReader {
    receiver: Receiver<Event>,
}

impl EventReader {
    fn new(receiver: Receiver<Event>) -> Self {
        Self { receiver }
    }

    /// Returns every event that has arrived since the last call, without blocking.
    fn take(&self) -> Vec<Event> {
        self.receiver.try_iter().collect()
    }
}

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub enum EventFilter {
    Duplicates,
    SessionId,
    Folder,
    ExitCodeSuccess,
}

impl Display for EventFilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::result::Result<(), std::fmt::Error> {
        match self {
            EventFilter::Duplicates => write!(f, "duplicates"),
            EventFilter::SessionId => write!(f, "session id"),
            EventFilter::Folder => write!(f, "folder"),
            EventFilter::ExitCodeSuccess => write!(f, "exit code success"),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct ParseEventFilterError(String);

impl std::fmt::Display for ParseEventFilterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid event filter: {}", self.0)
    }
}

impl std::error::Error for ParseEventFilterError {}

impl FromStr for EventFilter {
    type Err = ParseEventFilterError;
    fn from_str(filter: &str) -> std::result::Result<Self, Self::Err> {
        match filter {
            "duplicates" => Ok(EventFilter::Duplicates),
            "session_id" => Ok(EventFilter::SessionId),
            "folder" => Ok(EventFilter::Folder),
            "exit_code_success" => Ok(EventFilter::ExitCodeSuccess),
            _ => Err(ParseEventFilterError(filter.to_string())),
        }
    }
}

/// View after filtering Events
struct FilteredView<'a> {
    events: &'a [Event],
    indices: Vec<usize>,
}

impl<'a> FilteredView<'a> {
    fn build(
        events: &'a [Event],
        filters: &HashSet<EventFilter>,
        folder: &str,
        session_id: Option<&str>,
        dedup_map: &HashMap<String, usize>,
    ) -> Self {
        let indices = if filters.contains(&EventFilter::Duplicates) {
            let mut dedup_indices: Vec<usize> = dedup_map.values().copied().collect();
            dedup_indices.sort_unstable();
            dedup_indices
        } else {
            (0..events.len()).collect()
        };

        let indices = indices
            .into_iter()
            .filter(|&i| {
                #[expect(
                    clippy::indexing_slicing,
                    reason = "invariant by construction: i < self.events.len()"
                )]
                let event = &events[i];
                filters.iter().all(|f| match f {
                    EventFilter::Duplicates => true,
                    EventFilter::SessionId => session_id.is_none_or(|sid| event.session == sid),
                    EventFilter::Folder => event.folder == folder,
                    EventFilter::ExitCodeSuccess => event.exit_code == 0,
                })
            })
            .collect();

        Self { events, indices }
    }

    fn entries(&self) -> impl Iterator<Item = (usize, &str)> {
        #[expect(
            clippy::indexing_slicing,
            reason = "invariant by construction: i < self.events.len()"
        )]
        self.indices
            .iter()
            .map(|&i| (i, self.events[i].command.as_str()))
    }
}

pub struct Tui;

impl Tui {
    /// Set up the terminal and run the TUI. `receiver` is fed [`Event`]s by the caller.
    /// Returns the selected event, if any.
    ///
    /// # Errors
    ///
    /// Return an `Err` if setting up, drawing to or restoring the terminal fails.
    pub fn start(
        receiver: Receiver<Event>,
        query: &str,
        folder: &str,
        session_id: Option<String>,
        filters: HashSet<EventFilter>,
        show_score: bool,
    ) -> anyhow::Result<Option<Event>> {
        let reader = EventReader::new(receiver);
        let (mut terminal, terminal_events) = Tui::setup_terminal()?;
        let result = App::new(
            reader,
            query.to_string(),
            folder.to_string(),
            session_id,
            filters,
            show_score,
        )
        .run(&mut terminal, &terminal_events);
        let restored = Tui::restore_terminal(&mut terminal);
        // restore must happen before checking errors
        let selected = result?;
        restored?;
        Ok(selected)
    }

    fn setup_terminal() -> anyhow::Result<(
        Terminal<TerminaBackend<PlatformTerminal>>,
        TerminaEventReader,
    )> {
        let mut tty = PlatformTerminal::new()?;
        // hook is the only place to leave the alternate screen with panic=abort?
        tty.set_panic_hook(|handle| {
            if let Err(error) = write!(
                handle,
                "{}{}",
                decreset(DecPrivateModeCode::ClearAndEnableAlternateScreen),
                decset(DecPrivateModeCode::ShowCursor)
            ) {
                eprintln!("failed to restore terminal: {error}");
            }
        });
        tty.enter_raw_mode()?;
        let terminal_events = tty.event_reader();
        let mut terminal = Terminal::new(TerminaBackend::new(tty))?;
        // only now enter the alternate screen to avoid being stuck in alternate screen
        let backend = terminal.backend_mut();
        write!(
            backend,
            "{}",
            decset(DecPrivateModeCode::ClearAndEnableAlternateScreen)
        )?;
        backend.flush()?;
        Ok((terminal, terminal_events))
    }

    // raw mode is restored on drop of the underlying [`PlatformTerminal`]
    // this only needs to undo alternate screen and cursor visibility
    fn restore_terminal(
        terminal: &mut Terminal<TerminaBackend<PlatformTerminal>>,
    ) -> anyhow::Result<()> {
        let backend = terminal.backend_mut();
        write!(
            backend,
            "{}",
            decreset(DecPrivateModeCode::ClearAndEnableAlternateScreen)
        )?;
        terminal.show_cursor()?;
        terminal.backend_mut().flush()?;
        Ok(())
    }
}

fn decset(mode: DecPrivateModeCode) -> Csi {
    Csi::Mode(Mode::SetDecPrivateMode(DecPrivateMode::Code(mode)))
}

fn decreset(mode: DecPrivateModeCode) -> Csi {
    Csi::Mode(Mode::ResetDecPrivateMode(DecPrivateMode::Code(mode)))
}

fn layout(area: Rect) -> [Rect; 5] {
    area.layout(&Layout::vertical([
        Constraint::Min(1),    // history
        Constraint::Length(1), // spacing
        Constraint::Length(1), // status
        Constraint::Length(1), // input
        Constraint::Length(5), // preview
    ]))
}

fn visible_history_rows(area: Rect) -> usize {
    let [history_area, ..] = layout(area);
    history_area.height as usize
}

fn terminal_area(terminal: &Terminal<TerminaBackend<PlatformTerminal>>) -> anyhow::Result<Rect> {
    Ok(Rect::from((Position::ORIGIN, terminal.size()?)))
}

/// app holds the state of the application
struct App {
    /// current value of the input box
    input: String,
    /// position of cursor in the editor area.
    character_index: u16,
    /// indices into events sorted according to fuzzy score if we have a query
    indexer: Option<FuzzyIndex>,
    /// non-blocking drain of events sent by the loader thread
    reader: EventReader,
    /// accumulated events pool for filtering and matching
    events: Vec<Event>,
    /// currently selected index in the history widget (0 = bottom-most)
    selected_index: usize,
    /// currently active event filter
    filters: HashSet<EventFilter>,
    folder: String,
    session_id: Option<String>,
    show_score: bool,
    /// deduplicated list of entries (see [`EventFilter::Duplicates`])
    dedup_map: HashMap<String, usize>,
    /// commands removed from history; the loader may still send events loaded before removal
    removed: HashSet<String>,
}

impl App {
    fn new(
        reader: EventReader,
        query: String,
        folder: String,
        session_id: Option<String>,
        filters: HashSet<EventFilter>,
        show_score: bool,
    ) -> Self {
        let character_index = u16::try_from(query.chars().count()).unwrap_or(u16::MAX);
        Self {
            input: query,
            indexer: None,
            character_index,
            reader,
            events: Vec::new(),
            selected_index: 0,
            filters,
            folder,
            session_id,
            show_score,
            dedup_map: HashMap::new(),
            removed: HashSet::new(),
        }
    }

    fn collect_new_events(&mut self) {
        let mut new_events = self.reader.take();
        new_events.retain(|e| !self.removed.contains(&e.command));
        let base = self.events.len();
        for (i, e) in new_events.iter().enumerate() {
            self.dedup_map.entry(e.command.clone()).or_insert(base + i);
        }
        self.events.append(&mut new_events);
    }

    fn selected_event(&self) -> Option<&Event> {
        self.indexer
            .as_ref()
            .and_then(|indexer| indexer.get(self.selected_index))
            .and_then(|index| self.events.get(index))
    }

    /// remove all events of the selected command from the history files and the app state
    fn remove_selected(&mut self, visible_rows: usize) -> anyhow::Result<()> {
        let Some(command) = self.selected_event().map(|e| e.command.clone()) else {
            return Ok(());
        };
        crate::remove_command(&command)?;
        self.forget_command(command, visible_rows);
        Ok(())
    }

    /// drop all events of `command` while keeping the selection on the same row
    fn forget_command(&mut self, command: String, visible_rows: usize) {
        self.events.retain(|e| e.command != command);
        self.dedup_map.clear();
        for (i, e) in self.events.iter().enumerate() {
            self.dedup_map.entry(e.command.clone()).or_insert(i);
        }
        self.removed.insert(command);
        let selected_index = self.selected_index;
        self.run_matcher();
        self.selected_index = selected_index;
        self.clamp_selection(visible_rows);
    }

    fn run_matcher(&mut self) {
        let filtered = FilteredView::build(
            &self.events,
            &self.filters,
            &self.folder,
            self.session_id.as_deref(),
            &self.dedup_map,
        );
        let entries: Vec<(usize, &str)> = filtered.entries().collect();

        if self.input.is_empty() {
            // pass through (no score, no highlights)
            let matches: Vec<Match> = entries
                .iter()
                .map(|&(idx, _)| (idx, 0i64, vec![]))
                .collect();
            self.indexer = Some(FuzzyIndex::from(matches));
        } else {
            let matcher = FuzzyEngine::new(&self.input);
            let mut result = matcher.match_all(&entries);
            result.sort_unstable_by_key(|(idx, score, _)| {
                // TODO or just use min-length?
                #[expect(
                    clippy::indexing_slicing,
                    reason = "invariant by construction: idx < self.events.len()"
                )]
                let endtime = self.events[*idx].endtime;
                (std::cmp::Reverse(*score), std::cmp::Reverse(endtime))
            });
            self.indexer = Some(FuzzyIndex::from(result));
        }
        self.selected_index = 0;
    }

    fn move_cursor_left(&mut self) {
        let cursor_moved_left = self.character_index.saturating_sub(1);
        self.character_index = self.clamp_cursor(cursor_moved_left);
    }

    fn move_cursor_right(&mut self) {
        let cursor_moved_right = self.character_index.saturating_add(1);
        self.character_index = self.clamp_cursor(cursor_moved_right);
    }

    fn enter_char(&mut self, new_char: char) {
        let index = self.byte_index();
        self.input.insert(index, new_char);
        self.move_cursor_right();
        self.run_matcher();
    }

    /// returns the byte index for `character_index`, which counts Unicode scalar values not bytes.
    fn byte_index(&self) -> usize {
        self.input
            .char_indices()
            .map(|(i, _)| i)
            .nth(self.character_index as usize)
            .unwrap_or(self.input.len())
    }

    fn delete_char(&mut self) {
        let is_not_cursor_leftmost = self.character_index != 0;
        if is_not_cursor_leftmost {
            // String::remove works on bytes, not chars; reconstruct around the character instead.
            let current_index = self.character_index;
            let from_left_to_current_index = current_index - 1;
            let before_char_to_delete =
                self.input.chars().take(from_left_to_current_index as usize);
            let after_char_to_delete = self.input.chars().skip(current_index as usize);
            self.input = before_char_to_delete.chain(after_char_to_delete).collect();
            self.move_cursor_left();
        }

        self.run_matcher();
    }

    /// This mimics ctrl-w found in most terms
    fn delete_word(&mut self) {
        if self.character_index == 0 {
            return;
        }

        let byte_idx = self.byte_index();
        let before_cursor = &self.input[..byte_idx];

        // Trim trailing whitespace, then find last whitespace (word boundary)
        let trimmed = before_cursor.trim_end_matches(|c: char| c.is_whitespace());
        let word_start_byte = trimmed.rfind(char::is_whitespace).map_or(0, |i| {
            i + trimmed[i..].chars().next().map_or(0, char::len_utf8)
        });

        let word_start_char = self.input[..word_start_byte].chars().count();
        let before_word = self.input.chars().take(word_start_char);
        let after_cursor = self.input.chars().skip(self.character_index as usize);
        self.input = before_word.chain(after_cursor).collect();
        self.character_index =
            self.clamp_cursor(u16::try_from(word_start_char).unwrap_or(u16::MAX));

        self.run_matcher();
    }

    fn move_selection_up(&mut self, visible_rows: usize) {
        self.selected_index += 1;
        self.clamp_selection(visible_rows);
    }

    /// keeps the selection on a row that is both rendered and backed by a match
    fn clamp_selection(&mut self, visible_rows: usize) {
        let matches = self.indexer.as_ref().map_or(0, FuzzyIndex::len);
        let max_index = visible_rows.min(matches).saturating_sub(1);
        self.selected_index = self.selected_index.min(max_index);
    }

    fn move_selection_down(&mut self) {
        self.selected_index = self.selected_index.saturating_sub(1);
    }

    fn clamp_cursor(&self, new_cursor_pos: u16) -> u16 {
        new_cursor_pos.clamp(
            0,
            u16::try_from(self.input.chars().count()).unwrap_or(u16::MAX),
        )
    }

    fn toggle_filter(&mut self, event: EventFilter) {
        if self.filters.contains(&event) {
            self.filters.remove(&event);
        } else {
            self.filters.insert(event);
        }
    }

    fn active_filters(&self) -> String {
        // TODO find a nicer way to visualise the active filters
        self.filters
            .iter()
            .map(|filter| match filter {
                EventFilter::Duplicates => "U".to_string(),
                EventFilter::SessionId => "S".to_string(),
                EventFilter::Folder => "F".to_string(),
                EventFilter::ExitCodeSuccess => "E".to_string(),
            })
            .collect::<Vec<_>>()
            .join(" | ")
    }

    fn run(
        mut self,
        terminal: &mut Terminal<TerminaBackend<PlatformTerminal>>,
        terminal_events: &TerminaEventReader,
    ) -> anyhow::Result<Option<Event>> {
        self.collect_new_events();
        terminal.draw(|frame| self.render(frame))?;

        loop {
            if terminal_events.poll(Some(Duration::from_millis(100)), |_| true)? {
                match terminal_events.read(|_| true)? {
                    TerminaEvent::WindowResized(_) => {
                        self.clamp_selection(visible_history_rows(terminal_area(terminal)?));
                        terminal.draw(|frame| self.render(frame))?;
                    }
                    TerminaEvent::Key(key) if key.kind == KeyEventKind::Press => {
                        match (key.code, key.modifiers) {
                            (KeyCode::Enter, _) => {
                                return Ok(self.selected_event().cloned());
                            }
                            (KeyCode::Delete, KeyModifiers::SHIFT) => {
                                self.remove_selected(visible_history_rows(terminal_area(
                                    terminal,
                                )?))?;
                            }
                            (
                                KeyCode::Char(to_insert),
                                KeyModifiers::NONE | KeyModifiers::SHIFT,
                            ) => {
                                self.enter_char(to_insert);
                            }
                            (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                                self.toggle_filter(EventFilter::Duplicates);
                                self.run_matcher();
                            }
                            (KeyCode::Char('s'), KeyModifiers::CONTROL) => {
                                self.toggle_filter(EventFilter::SessionId);
                                self.run_matcher();
                            }
                            (KeyCode::Char('f'), KeyModifiers::CONTROL) => {
                                self.toggle_filter(EventFilter::Folder);
                                self.run_matcher();
                            }
                            (KeyCode::Char('e'), KeyModifiers::CONTROL) => {
                                self.toggle_filter(EventFilter::ExitCodeSuccess);
                                self.run_matcher();
                            }
                            (KeyCode::Char('x'), KeyModifiers::CONTROL) => {
                                self.show_score = !self.show_score;
                                self.run_matcher();
                            }
                            (KeyCode::Char('w'), KeyModifiers::CONTROL) => {
                                self.delete_word();
                            }
                            (KeyCode::Backspace, _) => self.delete_char(),
                            (KeyCode::Left, _) => self.move_cursor_left(),
                            (KeyCode::Right, _) => self.move_cursor_right(),
                            (KeyCode::Up, _) => {
                                self.move_selection_up(visible_history_rows(terminal_area(
                                    terminal,
                                )?));
                            }
                            (KeyCode::Down, _) => self.move_selection_down(),
                            (KeyCode::Escape, _)
                            | (KeyCode::Char('c' | 'd'), KeyModifiers::CONTROL) => {
                                return Ok(None);
                            }
                            _ => {}
                        }
                        terminal.draw(|frame| self.render(frame))?;
                    }
                    // TODO focus gained/lost?
                    _ => {}
                }
            } else {
                let events_before = self.events.len();
                self.collect_new_events();
                if self.events.len() != events_before {
                    self.run_matcher();
                    terminal.draw(|frame| self.render(frame))?;
                }
            }
        }
    }

    fn render_history(
        &self,
        indexer: &FuzzyIndex,
        num_items: usize,
        timeago_fn: impl Fn(&Event) -> String,
    ) -> Vec<ListItem<'_>> {
        indexer
            .first_n(num_items)
            .enumerate()
            .rev()
            .filter_map(|(i, idx)| {
                // TODO should always be Some(...): skip, report, log otherwise?
                let event = self.events.get(idx)?;
                let ago = timeago_fn(event);
                let command = &event.command;
                let mut spans = Vec::new();
                spans.push(Span::raw(format!("{ago} -- ")));
                if let Some(hl_indides) = indexer.highlight_indices(i) {
                    let mut last_index = 0;

                    for &char_index in hl_indides {
                        if char_index > last_index {
                            let text: String = command
                                .chars()
                                .skip(last_index)
                                .take(char_index - last_index)
                                .collect();
                            spans.push(Span::raw(text));
                        }

                        if let Some(char_to_highlight) = command.chars().nth(char_index) {
                            spans.push(Span::styled(
                                char_to_highlight.to_string(),
                                Style::default().fg(Color::Yellow),
                            ));
                            last_index = char_index + 1;
                        }
                    }

                    if last_index < command.len() {
                        let text: String = command.chars().skip(last_index).collect();
                        spans.push(Span::raw(text));
                    }
                } else {
                    spans.push(Span::raw(command.as_str()));
                }

                if self.show_score
                    && let Some(score) = indexer.matcher_score(i)
                {
                    spans.push(Span::raw(format!(" ({score})")));
                }

                let item = ListItem::new(Line::from(spans));
                if i == self.selected_index {
                    Some(item.style(Style::default().bg(Color::DarkGray)))
                } else {
                    Some(item)
                }
            })
            .collect()
    }

    fn render(&self, frame: &mut Frame) {
        let [history_area, _, status_area, input_area, preview_area] = layout(frame.area());
        let available_height = visible_history_rows(frame.area());

        let now = Utc::now().timestamp_millis();
        let timeago_fmt = timeago::Formatter::new();
        let timeago_fn = |event: &Event| {
            timeago_fmt.convert(std::time::Duration::from_millis(
                (now - event.endtime).cast_unsigned(),
            ))
        };
        let history: Vec<ListItem> = if let Some(indexer) = &self.indexer {
            self.render_history(indexer, available_height.min(self.events.len()), timeago_fn)
        } else {
            vec![]
        };
        let history_widget = List::new(history)
            .block(Block::default().padding(ratatui::widgets::Padding::horizontal(2)));
        frame.render_widget(history_widget, history_area);

        let filtered = if let Some(indexer) = &self.indexer {
            indexer.len()
        } else {
            0
        };
        let status_text = format!("{filtered}/{}", self.events.len());
        let status_line = Line::from(vec![Span::raw("  "), Span::raw(status_text)]);
        let filters = format!("[{}]  ", self.active_filters());
        let status_line_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Min(0),
                Constraint::Length(u16::try_from(filters.len()).unwrap_or(10)), // TODO default?
            ])
            .split(status_area);
        #[expect(clippy::indexing_slicing, reason = "constructed two constraints")]
        frame.render_widget(status_line, status_line_chunks[0]);
        #[expect(clippy::indexing_slicing, reason = "constructed two constraints")]
        frame.render_widget(filters, status_line_chunks[1]);

        let input_line = Line::from(vec![
            Span::raw("> "),
            Span::styled(
                self.input.as_str(),
                Style::default().fg(Color::Yellow).not_bold(),
            ),
        ]);
        let input = Paragraph::new(input_line).block(Block::default());
        frame.render_widget(input, input_area);
        // cursor does not exceed past the screen
        let cursor_column = input_area
            .x
            .saturating_add(2)
            .saturating_add(self.character_index)
            .min(input_area.right().saturating_sub(1));
        frame.set_cursor_position(Position::new(cursor_column, input_area.y));

        let preview_content = if let Some(indexer) = &self.indexer
            && let Some(idx) = indexer.get(self.selected_index)
        {
            if let Some(event) = self.events.get(idx) {
                format!("[exit code={}]: {}", event.exit_code, event.command)
            } else {
                String::new()
            }
        } else {
            String::new()
        };
        let preview = Paragraph::new(preview_content)
            .block(
                Block::default()
                    .borders(ratatui::widgets::Borders::TOP)
                    .border_style(Style::default().fg(Color::DarkGray)),
            )
            .wrap(ratatui::widgets::Wrap { trim: false });
        frame.render_widget(preview, preview_area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_app(input: &str) -> App {
        let character_index = u16::try_from(input.chars().count()).unwrap();
        App {
            input: input.to_string(),
            character_index,
            indexer: None,
            reader: EventReader::new(crossbeam_channel::unbounded().1),
            events: Vec::new(),
            selected_index: 0,
            filters: HashSet::new(),
            folder: String::new(),
            session_id: None,
            show_score: false,
            dedup_map: HashMap::new(),
            removed: HashSet::new(),
        }
    }

    fn with_matches(mut app: App, count: usize) -> App {
        let matches: Vec<Match> = (0..count).map(|index| (index, 0, vec![])).collect();
        app.indexer = Some(FuzzyIndex::from(matches));
        app
    }

    fn event(command: &str, endtime: i64) -> Event {
        Event {
            timestamp_millis: 0,
            command: command.to_string(),
            endtime,
            exit_code: 0,
            folder: String::new(),
            machine: String::new(),
            session: String::new(),
        }
    }

    #[test]
    fn forget_command_removes_all_occurrences() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut app = make_app("");
        app.reader = EventReader::new(rx);
        for (i, command) in ["ls", "secret", "pwd", "secret", "cd"].iter().enumerate() {
            tx.send(event(command, i64::try_from(i).unwrap())).unwrap();
        }
        app.collect_new_events();
        app.run_matcher();
        app.selected_index = 1;
        assert_eq!(app.selected_event().unwrap().command, "secret");

        app.forget_command("secret".to_string(), 10);
        let commands: Vec<&str> = app.events.iter().map(|e| e.command.as_str()).collect();
        assert_eq!(commands, ["ls", "pwd", "cd"]);
        assert_eq!(app.dedup_map.get("cd"), Some(&2));
        // selection stays on the same row
        assert_eq!(app.selected_index, 1);
        assert_eq!(app.selected_event().unwrap().command, "pwd");

        // events still in flight from the loader are dropped too
        tx.send(event("secret", 5)).unwrap();
        app.collect_new_events();
        assert_eq!(app.events.len(), 3);
    }

    #[test]
    fn delete_word_basic() {
        let mut app = make_app("hello world");
        app.delete_word();
        assert_eq!(app.input, "hello ");
        assert_eq!(app.character_index, 6);
    }

    #[test]
    fn delete_word_trailing_spaces() {
        let mut app = make_app("hello   ");
        app.delete_word();
        assert_eq!(app.input, "");
        assert_eq!(app.character_index, 0);
    }

    #[test]
    fn delete_word_single_word() {
        let mut app = make_app("hello");
        app.delete_word();
        assert_eq!(app.input, "");
        assert_eq!(app.character_index, 0);
    }

    #[test]
    fn delete_word_at_start_is_noop() {
        let mut app = make_app("hello");
        app.character_index = 0;
        app.delete_word();
        assert_eq!(app.input, "hello");
        assert_eq!(app.character_index, 0);
    }

    #[test]
    fn delete_word_mid_word() {
        let mut app = make_app("hello world");
        app.character_index = 7; // cursor between 'w' and 'o' in "world"
        app.delete_word();
        assert_eq!(app.input, "hello orld");
        assert_eq!(app.character_index, 6);
    }

    #[test]
    fn byte_index_ascii() {
        let app = make_app("hello");
        assert_eq!(app.byte_index(), 5);
    }

    #[test]
    fn byte_index_multibyte() {
        // "é" is 2 bytes; cursor after it should give byte index 2
        let mut app = make_app("é");
        app.character_index = 1;
        assert_eq!(app.byte_index(), 2);
    }

    #[test]
    fn byte_index_at_start() {
        let mut app = make_app("hello");
        app.character_index = 0;
        assert_eq!(app.byte_index(), 0);
    }

    #[test]
    fn toggle_filter_adds_and_removes() {
        let mut app = make_app("");
        app.toggle_filter(EventFilter::Duplicates);
        assert!(app.filters.contains(&EventFilter::Duplicates));
        app.toggle_filter(EventFilter::Duplicates);
        assert!(!app.filters.contains(&EventFilter::Duplicates));
    }

    #[test]
    fn active_filters_empty() {
        let app = make_app("");
        assert_eq!(app.active_filters(), "");
    }

    #[test]
    fn active_filters_shows_abbreviations() {
        let mut app = make_app("");
        app.toggle_filter(EventFilter::Duplicates);
        app.toggle_filter(EventFilter::ExitCodeSuccess);
        app.toggle_filter(EventFilter::Folder);
        app.toggle_filter(EventFilter::SessionId);
        let filters = app.active_filters();
        assert!(filters.contains('U'));
        assert!(filters.contains('E'));
        assert!(filters.contains('F'));
        assert!(filters.contains('S'));
    }

    #[test]
    fn move_selection_up_increments() {
        let mut app = with_matches(make_app(""), 10);
        app.move_selection_up(10);
        assert_eq!(app.selected_index, 1);
    }

    #[test]
    fn move_selection_up_clamps_to_visible_rows() {
        let mut app = with_matches(make_app(""), 10);
        app.selected_index = 4;
        app.move_selection_up(5);
        assert_eq!(app.selected_index, 4);
    }

    #[test]
    fn move_selection_up_clamps_to_matches() {
        let mut app = with_matches(make_app(""), 2);
        app.selected_index = 1;
        app.move_selection_up(10);
        assert_eq!(app.selected_index, 1);
    }

    #[test]
    fn move_selection_up_without_matches_stays_at_zero() {
        let mut app = make_app("");
        app.move_selection_up(10);
        assert_eq!(app.selected_index, 0);
    }

    #[test]
    fn clamp_selection_after_shrinking_rows() {
        let mut app = with_matches(make_app(""), 10);
        app.selected_index = 8;
        app.clamp_selection(3);
        assert_eq!(app.selected_index, 2);
    }

    #[test]
    fn visible_history_rows_matches_rendered_layout() {
        for total_rows in [3, 24] {
            let area = Rect::new(0, 0, 80, total_rows);
            let [_, padding, status_area, input_area, preview_area] = layout(area);
            let available_height = visible_history_rows(area);
            let occupied_ui_height =
                padding.height + status_area.height + input_area.height + preview_area.height;
            assert_eq!(
                available_height,
                total_rows.saturating_sub(occupied_ui_height) as usize
            );
        }
    }

    #[test]
    fn move_selection_down_decrements() {
        let mut app = make_app("");
        app.selected_index = 3;
        app.move_selection_down();
        assert_eq!(app.selected_index, 2);
    }

    #[test]
    fn move_selection_down_clamps_at_zero() {
        let mut app = make_app("");
        app.move_selection_down();
        assert_eq!(app.selected_index, 0);
    }

    #[test]
    fn enter_char_appends_at_end() {
        let mut app = make_app("hell");
        app.enter_char('o');
        assert_eq!(app.input, "hello");
        assert_eq!(app.character_index, 5);
    }

    #[test]
    fn enter_char_inserts_at_start() {
        let mut app = make_app("ello");
        app.character_index = 0;
        app.enter_char('h');
        assert_eq!(app.input, "hello");
        assert_eq!(app.character_index, 1);
    }

    #[test]
    fn enter_char_inserts_in_middle() {
        let mut app = make_app("hllo");
        app.character_index = 1;
        app.enter_char('e');
        assert_eq!(app.input, "hello");
        assert_eq!(app.character_index, 2);
    }

    #[test]
    fn enter_char_multibyte() {
        let mut app = make_app("h");
        app.enter_char('é');
        assert_eq!(app.input, "hé");
        assert_eq!(app.character_index, 2);
    }

    #[test]
    fn collect_new_events_drains_channel() {
        let (sender, receiver) = crossbeam_channel::unbounded();
        let mut app = make_app("");
        app.reader = EventReader::new(receiver);

        let event = Event {
            timestamp_millis: 0,
            command: "git status".to_string(),
            endtime: 1000,
            exit_code: 0,
            folder: "/".to_string(),
            machine: "m".to_string(),
            session: "s".to_string(),
        };
        sender.send(event).unwrap();
        drop(sender);

        app.collect_new_events();
        assert_eq!(app.events.len(), 1);
        assert_eq!(app.events[0].command, "git status");
    }

    #[test]
    fn move_cursor_left_decrements() {
        let mut app = make_app("hello");
        app.move_cursor_left();
        assert_eq!(app.character_index, 4);
    }

    #[test]
    fn move_cursor_left_clamps_at_zero() {
        let mut app = make_app("hello");
        app.character_index = 0;
        app.move_cursor_left();
        assert_eq!(app.character_index, 0);
    }

    #[test]
    fn move_cursor_right_increments() {
        let mut app = make_app("hello");
        app.character_index = 0;
        app.move_cursor_right();
        assert_eq!(app.character_index, 1);
    }

    #[test]
    fn move_cursor_right_clamps_at_end() {
        let mut app = make_app("hello");
        app.move_cursor_right();
        assert_eq!(app.character_index, 5);
    }

    #[test]
    fn delete_char_basic() {
        let mut app = make_app("hello");
        app.delete_char();
        assert_eq!(app.input, "hell");
        assert_eq!(app.character_index, 4);
    }

    #[test]
    fn delete_char_at_start_is_noop() {
        let mut app = make_app("hello");
        app.character_index = 0;
        app.delete_char();
        assert_eq!(app.input, "hello");
        assert_eq!(app.character_index, 0);
    }

    #[test]
    fn delete_char_multibyte() {
        let mut app = make_app("héllo");
        app.character_index = 2; // cursor after 'é'
        app.delete_char();
        assert_eq!(app.input, "hllo");
        assert_eq!(app.character_index, 1);
    }

    #[test]
    fn event_filter_from_str_roundtrip() {
        let cases = [
            ("duplicates", EventFilter::Duplicates),
            ("session_id", EventFilter::SessionId),
            ("folder", EventFilter::Folder),
            ("exit_code_success", EventFilter::ExitCodeSuccess),
        ];
        for (input, expected) in cases {
            assert_eq!(input.parse::<EventFilter>().unwrap(), expected);
        }
    }

    #[test]
    fn event_filter_from_str_unknown() {
        assert!("unknown".parse::<EventFilter>().is_err());
    }
}
