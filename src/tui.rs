use std::{
    collections::{BTreeMap, HashSet},
    io,
    sync::mpsc::{self, Receiver, TryRecvError},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};

use crate::{agent, catalog, profile, provider, settings};

type SyncResult = std::result::Result<Option<usize>, String>;

#[derive(Clone)]
struct AgentRow {
    agent: agent::Agent,
    values: BTreeMap<&'static str, String>,
    hidden: bool,
}

#[derive(Clone, Debug)]
struct PickerOption {
    value: String,
    note: String,
}

#[derive(Clone, Debug)]
enum PickerPurpose {
    Field {
        row: usize,
        key: &'static str,
        label: String,
    },
    Profiles,
}

struct Picker {
    purpose: PickerPurpose,
    title: String,
    placeholder: String,
    items: Vec<PickerOption>,
    matches: Vec<usize>,
    cursor: usize,
    input: TextInput,
    custom: bool,
}

impl Picker {
    fn new(
        purpose: PickerPurpose,
        title: String,
        placeholder: String,
        items: Vec<PickerOption>,
        custom: bool,
    ) -> Self {
        let mut picker = Self {
            purpose,
            title,
            placeholder,
            items,
            matches: Vec::new(),
            cursor: 0,
            input: TextInput::default(),
            custom,
        };
        picker.refilter();
        picker
    }

    fn refilter(&mut self) {
        let query = self.input.value.trim().to_lowercase();
        self.matches.clear();
        if query.is_empty() {
            self.matches.extend(0..self.items.len());
            self.cursor = self.cursor.min(self.matches.len().saturating_sub(1));
            return;
        }

        let mut hits = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| {
                search_score(&item.value, &item.note, &query).map(|score| (index, score))
            })
            .collect::<Vec<_>>();
        hits.sort_by_key(|(_, score)| *score);
        self.matches
            .extend(hits.into_iter().map(|(index, _)| index));
        self.cursor = 0;
    }

    fn choice(&self) -> Option<String> {
        self.matches
            .get(self.cursor)
            .map(|index| self.items[*index].value.clone())
            .or_else(|| {
                (self.custom && !self.input.value.trim().is_empty())
                    .then(|| self.input.value.trim().to_owned())
            })
    }

    fn move_cursor(&mut self, direction: isize) {
        let count = self.matches.len();
        if count == 0 {
            return;
        }
        self.cursor = self.cursor.wrapping_add_signed(direction).rem_euclid(count);
    }
}

#[derive(Default)]
struct TextInput {
    value: String,
    cursor: usize,
}

impl TextInput {
    fn insert(&mut self, character: char) {
        self.value.insert(self.cursor, character);
        self.cursor += character.len_utf8();
    }

    fn backspace(&mut self) {
        if let Some((index, _)) = self.value[..self.cursor].char_indices().next_back() {
            self.value.drain(index..self.cursor);
            self.cursor = index;
        }
    }

    fn delete(&mut self) {
        if let Some(character) = self.value[self.cursor..].chars().next() {
            let end = self.cursor + character.len_utf8();
            self.value.drain(self.cursor..end);
        }
    }

    fn left(&mut self) {
        if let Some((index, _)) = self.value[..self.cursor].char_indices().next_back() {
            self.cursor = index;
        }
    }

    fn right(&mut self) {
        if let Some(character) = self.value[self.cursor..].chars().next() {
            self.cursor += character.len_utf8();
        }
    }

    fn display(&self, placeholder: &str) -> Line<'_> {
        if self.value.is_empty() && !placeholder.is_empty() {
            return Line::from(Span::styled(
                placeholder.to_owned(),
                Style::default().fg(Color::DarkGray),
            ));
        }
        let (before, after) = self.value.split_at(self.cursor);
        Line::from(vec![
            Span::raw(before.to_owned()),
            Span::styled("▏", Style::default().fg(Color::Cyan)),
            Span::raw(after.to_owned()),
        ])
    }

    fn handle(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            if matches!(key.code, KeyCode::Char('u')) {
                self.value.clear();
                self.cursor = 0;
            }
            return;
        }
        match key.code {
            KeyCode::Char(character) => self.insert(character),
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete => self.delete(),
            KeyCode::Left => self.left(),
            KeyCode::Right => self.right(),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.value.len(),
            _ => {}
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Screen {
    Agents,
    Picker,
    SaveProfile,
}

struct App {
    rows: Vec<AgentRow>,
    row: usize,
    column: usize,
    screen: Screen,
    picker: Option<Picker>,
    name: TextInput,
    preview: Vec<(String, String)>,
    status: String,
    status_ok: bool,
    syncing: bool,
    sync_receiver: Option<Receiver<SyncResult>>,
    quit: bool,
}

pub async fn command(args: &[String]) -> Result<()> {
    if !args.is_empty() {
        bail!("usage: magpie tui");
    }
    let mut app = App::new()?;
    if catalog::is_stale() {
        app.start_catalog_sync(false);
    }
    ratatui::run(|terminal| app.run(terminal)).context("run terminal interface")
}

impl App {
    fn new() -> Result<Self> {
        let preferences = settings::load();
        let mut agents = agent::all()
            .into_iter()
            .filter(agent::Agent::is_detected)
            .collect::<Vec<_>>();
        agents.sort_by_key(|current| {
            (
                preferences
                    .agents_hidden
                    .iter()
                    .any(|id| id == current.spec.id),
                preferences
                    .agent_order
                    .iter()
                    .position(|id| id == current.spec.id)
                    .unwrap_or(usize::MAX),
            )
        });
        if agents.is_empty() {
            bail!("no supported agents found on this machine");
        }

        let rows = agents
            .into_iter()
            .map(|current| {
                Ok(AgentRow {
                    values: values_for(&current)?,
                    hidden: preferences
                        .agents_hidden
                        .iter()
                        .any(|id| id == current.spec.id),
                    agent: current,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        Ok(Self {
            rows,
            row: 0,
            column: 0,
            screen: Screen::Agents,
            picker: None,
            name: TextInput::default(),
            preview: Vec::new(),
            status: String::new(),
            status_ok: true,
            syncing: false,
            sync_receiver: None,
            quit: false,
        })
    }

    fn run(&mut self, terminal: &mut DefaultTerminal) -> io::Result<()> {
        while !self.quit {
            self.receive_sync_result();
            terminal.draw(|frame| self.draw(frame))?;
            if event::poll(Duration::from_millis(100))?
                && let Event::Key(key) = event::read()?
            {
                self.handle_key(key);
            }
        }
        Ok(())
    }

    fn draw(&self, frame: &mut Frame<'_>) {
        let chunks = Layout::default()
            .direction(ratatui::layout::Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(1),
                Constraint::Length(2),
            ])
            .split(frame.area());

        let title = match self.screen {
            Screen::Agents => "◉ magpie",
            Screen::Picker => self
                .picker
                .as_ref()
                .map_or("◉ magpie", |picker| picker.title.as_str()),
            Screen::SaveProfile => "◉ magpie › save profile",
        };
        frame.render_widget(
            Paragraph::new(Span::styled(
                title.to_owned(),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ))
            .block(Block::default().borders(Borders::BOTTOM)),
            chunks[0],
        );

        match self.screen {
            Screen::Agents => self.draw_agents(frame, chunks[1]),
            Screen::Picker => self.draw_picker(frame, chunks[1]),
            Screen::SaveProfile => self.draw_save_profile(frame, chunks[1]),
        }
        self.draw_footer(frame, chunks[2]);
    }

    fn draw_agents(&self, frame: &mut Frame<'_>, area: Rect) {
        let items = self
            .rows
            .iter()
            .map(|row| {
                let mut spans = vec![Span::styled(
                    row.agent.spec.name.to_owned(),
                    if row.hidden {
                        Style::default().fg(Color::DarkGray)
                    } else {
                        Style::default().add_modifier(Modifier::BOLD)
                    },
                )];
                if row.agent.spec.fields.is_empty() {
                    spans.push(Span::styled(
                        "  not configurable yet".to_owned(),
                        Style::default().fg(Color::DarkGray),
                    ));
                } else {
                    for (column, field) in row.agent.spec.fields.iter().enumerate() {
                        let value = row
                            .values
                            .get(field.key)
                            .map_or("—", |value| if value.is_empty() { "—" } else { value });
                        let label = if column == 0 && field.label == "model" {
                            String::new()
                        } else {
                            format!("{}: ", field.label)
                        };
                        let text = format!("  {label}{value}");
                        let style = if row.hidden {
                            Style::default().fg(Color::DarkGray)
                        } else {
                            Style::default()
                        };
                        spans.push(Span::styled(text, style));
                    }
                }
                ListItem::new(Line::from(spans))
            })
            .collect::<Vec<_>>();
        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title("Agents"))
            .highlight_symbol("› ")
            .highlight_style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            );
        let mut state = ListState::default();
        state.select(Some(self.row));
        frame.render_stateful_widget(list, area, &mut state);
    }

    fn draw_picker(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(picker) = self.picker.as_ref() else {
            return;
        };
        let chunks = Layout::default()
            .direction(ratatui::layout::Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(1)])
            .split(area);
        frame.render_widget(
            Paragraph::new(picker.input.display(&picker.placeholder))
                .block(Block::default().borders(Borders::ALL).title("Filter")),
            chunks[0],
        );

        if picker.matches.is_empty() {
            let message = if picker.custom && !picker.input.value.trim().is_empty() {
                format!(
                    "Press Enter to use {:?} as a custom value",
                    picker.input.value.trim()
                )
            } else if picker.items.is_empty() {
                "No choices are available yet".to_owned()
            } else {
                "No matching choices".to_owned()
            };
            frame.render_widget(
                Paragraph::new(message)
                    .style(Style::default().fg(Color::DarkGray))
                    .block(Block::default().borders(Borders::ALL).title("Choices")),
                chunks[1],
            );
            return;
        }

        let items = picker
            .matches
            .iter()
            .map(|index| {
                let item = &picker.items[*index];
                let mut spans = vec![Span::styled(
                    item.value.clone(),
                    Style::default().add_modifier(Modifier::BOLD),
                )];
                if !item.note.is_empty() {
                    spans.push(Span::styled(
                        format!("  {}", item.note),
                        Style::default().fg(Color::DarkGray),
                    ));
                }
                ListItem::new(Line::from(spans))
            })
            .collect::<Vec<_>>();
        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title("Choices"))
            .highlight_symbol("› ")
            .highlight_style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            );
        let mut state = ListState::default();
        state.select(Some(picker.cursor));
        frame.render_stateful_widget(list, chunks[1], &mut state);
    }

    fn draw_save_profile(&self, frame: &mut Frame<'_>, area: Rect) {
        let chunks = Layout::default()
            .direction(ratatui::layout::Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(1)])
            .split(area);
        frame.render_widget(
            Paragraph::new(self.name.display("profile name"))
                .block(Block::default().borders(Borders::ALL).title("Save profile")),
            chunks[0],
        );
        let items = self
            .preview
            .iter()
            .map(|(key, value)| {
                ListItem::new(Line::from(vec![
                    Span::styled(key.clone(), Style::default().fg(Color::DarkGray)),
                    Span::raw(format!("  {value}")),
                ]))
            })
            .collect::<Vec<_>>();
        frame.render_widget(
            List::new(items).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!("Current settings · {}", self.preview.len())),
            ),
            chunks[1],
        );
    }

    fn draw_footer(&self, frame: &mut Frame<'_>, area: Rect) {
        let status = if !self.status.is_empty() {
            Span::styled(
                self.status.clone(),
                Style::default().fg(if self.status_ok {
                    Color::Green
                } else {
                    Color::Red
                }),
            )
        } else if self.syncing {
            Span::styled(
                "… syncing model catalog",
                Style::default().fg(Color::Yellow),
            )
        } else {
            Span::styled("Ready", Style::default().fg(Color::DarkGray))
        };
        let hints = match self.screen {
            Screen::Agents => "↑↓ agent   ←→ field   Enter change   s save   p profiles   q quit",
            Screen::Picker
                if self
                    .picker
                    .as_ref()
                    .is_some_and(|picker| matches!(&picker.purpose, PickerPurpose::Profiles)) =>
            {
                "Type to filter   ↑↓ move   Enter apply   Ctrl+D delete   Esc back"
            }
            Screen::Picker => "Type to filter   ↑↓ move   Enter select   Esc back",
            Screen::SaveProfile => "Type a name   Enter save   Esc cancel",
        };
        frame.render_widget(
            Paragraph::new(Text::from(vec![
                Line::from(status),
                Line::from(Span::styled(hints, Style::default().fg(Color::DarkGray))),
            ]))
            .block(Block::default().borders(Borders::TOP)),
            area,
        );
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.quit = true;
            return;
        }
        match self.screen {
            Screen::Agents => self.handle_agent_key(key),
            Screen::Picker => self.handle_picker_key(key),
            Screen::SaveProfile => self.handle_save_key(key),
        }
    }

    fn handle_agent_key(&mut self, key: KeyEvent) {
        self.status.clear();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.quit = true,
            KeyCode::Down | KeyCode::Char('j') => {
                self.row = (self.row + 1) % self.rows.len();
                self.column = 0;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.row = (self.row + self.rows.len() - 1) % self.rows.len();
                self.column = 0;
            }
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Tab => {
                let fields = self.rows[self.row].agent.spec.fields.len();
                if fields > 0 {
                    self.column = (self.column + 1) % fields;
                }
            }
            KeyCode::Left | KeyCode::Char('h') | KeyCode::BackTab => {
                let fields = self.rows[self.row].agent.spec.fields.len();
                if fields > 0 {
                    self.column = (self.column + fields - 1) % fields;
                }
            }
            KeyCode::Enter | KeyCode::Char(' ') => self.open_field_picker(),
            KeyCode::Char('r') => self.reload_values(),
            KeyCode::Char('S') => self.start_catalog_sync(true),
            KeyCode::Char('s') => self.open_save_profile(),
            KeyCode::Char('p') => self.open_profiles(),
            _ => {}
        }
    }

    fn handle_picker_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.picker = None;
                self.screen = Screen::Agents;
            }
            KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(picker) = self.picker.as_mut() {
                    picker.move_cursor(1);
                }
            }
            KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(picker) = self.picker.as_mut() {
                    picker.move_cursor(-1);
                }
            }
            KeyCode::Down => {
                if let Some(picker) = self.picker.as_mut() {
                    picker.move_cursor(1);
                }
            }
            KeyCode::Up => {
                if let Some(picker) = self.picker.as_mut() {
                    picker.move_cursor(-1);
                }
            }
            KeyCode::Enter => self.choose_picker_option(),
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.delete_selected_profile()
            }
            _ => {
                if let Some(picker) = self.picker.as_mut() {
                    picker.input.handle(key);
                    picker.refilter();
                }
            }
        }
    }

    fn handle_save_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.screen = Screen::Agents,
            KeyCode::Enter => {
                let name = self.name.value.trim().to_owned();
                if name.is_empty() {
                    self.set_status("Profile name is empty", false);
                    return;
                }
                match profile::save_named(&name) {
                    Ok(()) => {
                        self.set_status(&format!("Saved profile {name}"), true);
                        self.screen = Screen::Agents;
                    }
                    Err(error) => self.set_status(&format!("{error:#}"), false),
                }
            }
            _ => self.name.handle(key),
        }
    }

    fn open_field_picker(&mut self) {
        let row = self.row;
        let Some(field) = self.rows[row].agent.spec.fields.get(self.column).copied() else {
            self.set_status("This agent has no editable fields yet", false);
            return;
        };
        let mut items = field_options(field);
        if let Err(error) = add_model_options(field, &mut items) {
            self.set_status(&format!("Could not load model choices: {error:#}"), false);
            return;
        }
        let current = self.rows[row]
            .values
            .get(field.key)
            .cloned()
            .unwrap_or_default();
        if !current.is_empty() {
            if let Some(item) = items.iter_mut().find(|item| item.value == current) {
                if item.note.is_empty() {
                    item.note = "current".to_owned();
                } else {
                    item.note.push_str(" · current");
                }
            } else {
                items.insert(
                    0,
                    PickerOption {
                        value: current,
                        note: "current".to_owned(),
                    },
                );
            }
        }
        let agent_name = self.rows[row].agent.spec.name;
        self.picker = Some(Picker::new(
            PickerPurpose::Field {
                row,
                key: field.key,
                label: format!("{} · {}", agent_name, field.label),
            },
            format!("◉ magpie › {} › {}", agent_name, field.label),
            "type to filter, or enter a custom value".to_owned(),
            items,
            true,
        ));
        self.screen = Screen::Picker;
    }

    fn open_profiles(&mut self) {
        match profile::list_entries() {
            Ok(entries) => {
                let items = entries
                    .into_iter()
                    .map(|(value, note)| PickerOption { value, note })
                    .collect();
                self.picker = Some(Picker::new(
                    PickerPurpose::Profiles,
                    "◉ magpie › profiles".to_owned(),
                    "filter profiles".to_owned(),
                    items,
                    false,
                ));
                self.screen = Screen::Picker;
                self.status.clear();
            }
            Err(error) => self.set_status(&format!("{error:#}"), false),
        }
    }

    fn open_save_profile(&mut self) {
        self.name = TextInput::default();
        match profile::snapshot_entries() {
            Ok(preview) => self.preview = preview,
            Err(error) => {
                self.set_status(
                    &format!("Could not read current settings: {error:#}"),
                    false,
                );
                return;
            }
        }
        self.screen = Screen::SaveProfile;
    }

    fn choose_picker_option(&mut self) {
        let Some(picker) = self.picker.take() else {
            return;
        };
        let Some(value) = picker.choice() else {
            return;
        };
        self.screen = Screen::Agents;
        match picker.purpose {
            PickerPurpose::Field { row, key, label } => match self.rows[row].agent.set(key, &value)
            {
                Ok(()) => {
                    self.reload_values();
                    self.set_status(&format!("{label} → {value}"), true);
                }
                Err(error) => self.set_status(&format!("{error:#}"), false),
            },
            PickerPurpose::Profiles => match profile::apply_named(&value) {
                Ok(changed) => {
                    self.reload_values();
                    self.set_status(
                        &format!("Applied profile {value} · {changed} settings changed"),
                        true,
                    );
                }
                Err(error) => self.set_status(&format!("{error:#}"), false),
            },
        }
    }

    fn delete_selected_profile(&mut self) {
        let Some(picker) = self.picker.as_ref() else {
            return;
        };
        if !matches!(&picker.purpose, PickerPurpose::Profiles) {
            return;
        }
        let Some(name) = picker.choice() else {
            return;
        };
        match profile::delete_named(&name) {
            Ok(()) => {
                self.open_profiles();
                self.set_status(&format!("Deleted profile {name}"), true);
            }
            Err(error) => self.set_status(&format!("{error:#}"), false),
        }
    }

    fn reload_values(&mut self) {
        let mut failure = None;
        for row in &mut self.rows {
            match values_for(&row.agent) {
                Ok(values) => row.values = values,
                Err(error) => {
                    failure = Some(format!(
                        "Could not reload {}: {error:#}",
                        row.agent.spec.name
                    ));
                    break;
                }
            }
        }
        match failure {
            Some(message) => self.set_status(&message, false),
            None => self.set_status("Reloaded settings", true),
        }
    }

    fn start_catalog_sync(&mut self, force: bool) {
        if self.syncing {
            return;
        }
        let (sender, receiver) = mpsc::channel();
        let result_sender = sender;
        let sync = async move {
            let result = if force {
                catalog::sync_models_dev().await.map(Some)
            } else {
                catalog::sync_if_stale().await
            }
            .map_err(|error| format!("{error:#}"));
            let _ = result_sender.send(result);
        };
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn(sync);
                self.sync_receiver = Some(receiver);
                self.syncing = true;
                self.status.clear();
            }
            Err(error) => self.set_status(
                &format!("Catalog sync needs an async runtime: {error}"),
                false,
            ),
        }
    }

    fn receive_sync_result(&mut self) {
        let result = match self
            .sync_receiver
            .as_ref()
            .map(|receiver| receiver.try_recv())
        {
            Some(Ok(result)) => Some(result),
            Some(Err(TryRecvError::Disconnected)) => {
                Some(Err("catalog sync worker stopped".to_owned()))
            }
            Some(Err(TryRecvError::Empty)) | None => None,
        };
        let Some(result) = result else {
            return;
        };
        self.sync_receiver = None;
        self.syncing = false;
        match result {
            Ok(Some(count)) => {
                self.set_status(&format!("Model catalog synced for {count} providers"), true)
            }
            Ok(None) => {}
            Err(error) => self.set_status(&format!("Catalog sync failed: {error}"), false),
        }
    }

    fn set_status(&mut self, message: &str, ok: bool) {
        self.status = message.to_owned();
        self.status_ok = ok;
    }
}

fn values_for(current: &agent::Agent) -> Result<BTreeMap<&'static str, String>> {
    current.values().map(|values| values.into_iter().collect())
}

fn field_options(field: &agent::FieldSpec) -> Vec<PickerOption> {
    field
        .choices
        .iter()
        .map(|choice| PickerOption {
            value: (*choice).to_owned(),
            note: String::new(),
        })
        .collect()
}

fn add_model_options(field: &agent::FieldSpec, items: &mut Vec<PickerOption>) -> Result<()> {
    if field.key != "model" && field.key != "small" {
        return Ok(());
    }
    let mut seen = items
        .iter()
        .map(|item| item.value.clone())
        .collect::<HashSet<_>>();
    for entry in provider::available_model_entries()? {
        if seen.insert(entry.id.clone()) {
            items.push(PickerOption {
                value: entry.id,
                note: format!("{} · {}", entry.provider_name, entry.model.name),
            });
        }
    }
    for group in provider::groups()?
        .into_iter()
        .filter(|group| !group.hidden)
    {
        let value = format!("group/{}", group.id);
        if seen.insert(value.clone()) {
            items.push(PickerOption {
                value,
                note: format!("routing group · {} models", group.members.len()),
            });
        }
    }
    Ok(())
}

fn search_score(value: &str, note: &str, query: &str) -> Option<usize> {
    let value = value.to_lowercase();
    let note = note.to_lowercase();
    if value.starts_with(query) {
        return Some(0);
    }
    if let Some(index) = value.find(query) {
        return Some(1 + index);
    }
    let mut chars = value.chars();
    if query
        .chars()
        .all(|needle| chars.by_ref().any(|character| character == needle))
    {
        return Some(1000);
    }
    note.contains(query).then_some(2000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picker_prioritizes_prefix_and_substring_matches() {
        assert_eq!(search_score("gpt-5.6", "OpenAI", "gpt"), Some(0));
        assert_eq!(search_score("my-gpt-5.6", "OpenAI", "gpt"), Some(4));
        assert_eq!(search_score("claude", "OpenAI GPT", "gpt"), Some(2000));
        assert_eq!(search_score("claude", "Anthropic", "missing"), None);
    }

    #[test]
    fn picker_input_edits_unicode_without_splitting_characters() {
        let mut input = TextInput::default();
        input.insert('猫');
        input.insert('a');
        input.left();
        input.backspace();
        assert_eq!(input.value, "a");
        assert_eq!(input.cursor, 0);
    }

    #[test]
    fn custom_picker_values_are_used_when_search_has_no_match() {
        let mut picker = Picker::new(
            PickerPurpose::Profiles,
            "profiles".to_owned(),
            "filter".to_owned(),
            vec![PickerOption {
                value: "work".to_owned(),
                note: String::new(),
            }],
            true,
        );
        picker.input.value = "new-profile".to_owned();
        picker.input.cursor = picker.input.value.len();
        picker.refilter();
        assert_eq!(picker.choice().as_deref(), Some("new-profile"));
    }
}
