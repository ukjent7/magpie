use std::{
    collections::{BTreeMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, TryRecvError},
    },
    time::Duration,
};

use anyhow::{Context, Result};
use eframe::egui::{self, RichText};
use tray_icon::{
    Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent,
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
};

use crate::{agent, catalog, provider, settings};

type SyncResult = std::result::Result<Option<usize>, String>;

#[derive(Default)]
struct Signals {
    open: Arc<AtomicBool>,
    toggle: Arc<AtomicBool>,
    quit: Arc<AtomicBool>,
}

struct AgentRow {
    agent: agent::Agent,
    values: BTreeMap<&'static str, String>,
    drafts: BTreeMap<&'static str, String>,
}

#[derive(Clone)]
struct ModelChoice {
    value: String,
    label: String,
    provider: String,
}

enum Action {
    SetField {
        row: usize,
        field: &'static str,
        value: String,
    },
    Reload,
    Sync,
    Quit,
}

pub async fn command(start_hidden: bool) -> Result<()> {
    settings::migrate();
    agent::sync_catalog_models()?;
    let gateway = crate::gateway::start_background().await?;
    let icon = window_icon_data()?;
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("magpie")
            .with_inner_size([1024.0, 720.0])
            .with_min_inner_size([760.0, 520.0])
            .with_icon(Arc::new(icon))
            .with_visible(!start_hidden),
        ..Default::default()
    };

    let ui_result = eframe::run_native(
        "magpie",
        native_options,
        Box::new(move |creation| {
            let app = App::new(creation.egui_ctx.clone(), start_hidden)?;
            Ok(Box::new(app))
        }),
    )
    .map_err(|error| anyhow::anyhow!("run desktop interface: {error:?}"));

    let gateway_result = match gateway {
        Some(gateway) => gateway.shutdown().await,
        None => Ok(()),
    };
    ui_result.and(gateway_result)
}

struct App {
    rows: Vec<AgentRow>,
    selected: usize,
    model_choices: Vec<ModelChoice>,
    signals: Signals,
    _tray: Option<TrayIcon>,
    visible: bool,
    exiting: bool,
    status: String,
    status_ok: bool,
    syncing: bool,
    sync_receiver: Option<Receiver<SyncResult>>,
}

impl App {
    fn new(ctx: egui::Context, start_hidden: bool) -> Result<Self> {
        let preferences = settings::load();
        match preferences.theme.as_str() {
            "light" => ctx.set_visuals(egui::Visuals::light()),
            "dark" => ctx.set_visuals(egui::Visuals::dark()),
            _ => {}
        }

        let mut agents = agent::all()
            .into_iter()
            .filter(agent::Agent::is_detected)
            .collect::<Vec<_>>();
        agents.sort_by_key(|current| {
            preferences
                .agent_order
                .iter()
                .position(|id| id == current.spec.id)
                .unwrap_or(usize::MAX)
        });

        let mut status = String::from("Ready");
        let mut status_ok = true;
        let mut rows = Vec::with_capacity(agents.len());
        for current in agents {
            match values_for(&current) {
                Ok(values) => rows.push(AgentRow {
                    drafts: values.clone(),
                    values,
                    agent: current,
                }),
                Err(error) => {
                    status = format!("Could not read {} settings: {error:#}", current.spec.name);
                    status_ok = false;
                    rows.push(AgentRow {
                        agent: current,
                        values: BTreeMap::new(),
                        drafts: BTreeMap::new(),
                    });
                }
            }
        }

        let signals = Signals::default();
        let (tray, tray_error) = match create_tray(&ctx, &signals) {
            Ok(tray) => (Some(tray), None),
            Err(error) if start_hidden => return Err(error),
            Err(error) => (None, Some(format!("System tray unavailable: {error:#}"))),
        };
        if let Some(error) = tray_error {
            status = error;
            status_ok = false;
        }

        let model_choices = match model_choices() {
            Ok(choices) => choices,
            Err(error) => {
                status = format!("Could not load model choices: {error:#}");
                status_ok = false;
                Vec::new()
            }
        };
        let mut app = Self {
            rows,
            selected: 0,
            model_choices,
            signals,
            _tray: tray,
            visible: !start_hidden,
            exiting: false,
            status,
            status_ok,
            syncing: false,
            sync_receiver: None,
        };
        if catalog::is_stale() {
            app.start_catalog_sync(false);
        }
        Ok(app)
    }

    fn process_events(&mut self, ctx: &egui::Context) {
        if self.signals.quit.swap(false, Ordering::Relaxed) {
            self.exiting = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        } else if self.signals.open.swap(false, Ordering::Relaxed) {
            self.set_visible(ctx, true);
        } else if self.signals.toggle.swap(false, Ordering::Relaxed) {
            self.set_visible(ctx, !self.visible);
        }

        if ctx.input(|input| input.viewport().close_requested())
            && !self.exiting
            && self._tray.is_some()
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.set_visible(ctx, false);
        }
        self.receive_sync_result();
    }

    fn set_visible(&mut self, ctx: &egui::Context, visible: bool) {
        self.visible = visible;
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(visible));
        if visible {
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
    }

    fn render_header(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        ui.horizontal(|ui| {
            ui.heading(RichText::new("magpie").strong());
            ui.add_space(12.0);
            ui.label("Your agents, one model switchboard");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Quit").clicked() {
                    actions.push(Action::Quit);
                }
                if ui
                    .add_enabled(!self.syncing, egui::Button::new("Refresh catalog"))
                    .clicked()
                {
                    actions.push(Action::Sync);
                }
                if ui.button("Reload settings").clicked() {
                    actions.push(Action::Reload);
                }
            });
        });
        ui.label(
            RichText::new(format!("Local gateway  ·  {}", crate::gateway::url()))
                .small()
                .color(ui.visuals().weak_text_color()),
        );
    }

    fn render_sidebar(&self, ui: &mut egui::Ui, selected: &mut usize) {
        ui.heading("Agents");
        ui.add_space(8.0);
        if self.rows.is_empty() {
            ui.label("No supported coding agents detected yet.");
            ui.add_space(8.0);
            ui.label("Install an agent, then reload settings.");
            return;
        }
        egui::ScrollArea::vertical().show(ui, |ui| {
            for (index, row) in self.rows.iter().enumerate() {
                let label = if row.values.is_empty() {
                    format!("{}  ·  needs attention", row.agent.spec.name)
                } else {
                    row.agent.spec.name.to_owned()
                };
                if ui.selectable_label(*selected == index, label).clicked() {
                    *selected = index;
                }
            }
        });
    }

    fn render_agent(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        let Some(row) = self.rows.get_mut(self.selected) else {
            ui.vertical_centered(|ui| {
                ui.add_space(80.0);
                ui.heading("Welcome to magpie");
                ui.label("Supported agents will appear here when installed.");
            });
            return;
        };

        ui.horizontal(|ui| {
            ui.heading(row.agent.spec.name);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    RichText::new("Detected")
                        .small()
                        .color(egui::Color32::from_rgb(88, 190, 128)),
                );
            });
        });
        ui.label(
            RichText::new(row.agent.path.display().to_string())
                .small()
                .color(ui.visuals().weak_text_color()),
        );
        ui.add_space(20.0);

        let row_index = self.selected;
        let agent_id = row.agent.spec.id.to_owned();
        let model_choices = &self.model_choices;
        egui::ScrollArea::vertical().show(ui, |ui| {
            for field in row.agent.spec.fields {
                let saved = row.values.get(field.key).cloned().unwrap_or_default();
                let draft = row.drafts.entry(field.key).or_insert_with(|| saved.clone());
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.vertical(|ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(field.label).strong());
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if !saved.is_empty() && ui.small_button("Default").clicked() {
                                        actions.push(Action::SetField {
                                            row: row_index,
                                            field: field.key,
                                            value: String::new(),
                                        });
                                    }
                                },
                            );
                        });

                        if field.choices.is_empty() {
                            ui.horizontal(|ui| {
                                let width = ui.available_width().min(420.0);
                                let response = ui.add_sized(
                                    [width, 30.0],
                                    egui::TextEdit::singleline(draft)
                                        .hint_text("Enter a model or value"),
                                );
                                if response.lost_focus()
                                    && ui.input(|input| input.key_pressed(egui::Key::Enter))
                                    && *draft != saved
                                {
                                    actions.push(Action::SetField {
                                        row: row_index,
                                        field: field.key,
                                        value: draft.clone(),
                                    });
                                }
                                if *draft != saved && ui.button("Apply").clicked() {
                                    actions.push(Action::SetField {
                                        row: row_index,
                                        field: field.key,
                                        value: draft.clone(),
                                    });
                                }
                            });

                            if !field.catalog_prefix.is_empty() && !model_choices.is_empty() {
                                egui::ComboBox::from_id_salt((agent_id.as_str(), field.key))
                                    .selected_text("Choose a gateway model…")
                                    .height(300.0)
                                    .show_ui(ui, |ui| {
                                        for choice in model_choices {
                                            let value =
                                                format!("{}{}", field.catalog_prefix, choice.value);
                                            let label =
                                                format!("{}  ·  {}", choice.label, choice.provider);
                                            if ui.selectable_label(*draft == value, label).clicked()
                                            {
                                                *draft = value;
                                            }
                                        }
                                    });
                            }
                        } else {
                            let mut choice = draft.clone();
                            egui::ComboBox::from_id_salt((agent_id.as_str(), field.key))
                                .selected_text(if choice.is_empty() {
                                    "Default"
                                } else {
                                    choice.as_str()
                                })
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(&mut choice, String::new(), "Default");
                                    for option in field.choices {
                                        ui.selectable_value(
                                            &mut choice,
                                            (*option).to_owned(),
                                            *option,
                                        );
                                    }
                                });
                            if choice != *draft {
                                *draft = choice.clone();
                            }
                            if *draft != saved {
                                ui.horizontal(|ui| {
                                    if ui.button("Apply").clicked() {
                                        actions.push(Action::SetField {
                                            row: row_index,
                                            field: field.key,
                                            value: draft.clone(),
                                        });
                                    }
                                });
                            }
                        }
                    });
                });
                ui.add_space(10.0);
            }
        });
    }

    fn apply_action(&mut self, action: Action, ctx: &egui::Context) {
        match action {
            Action::SetField { row, field, value } => {
                let Some(entry) = self.rows.get_mut(row) else {
                    return;
                };
                let agent_name = entry.agent.spec.name.to_owned();
                match entry.agent.set(field, &value) {
                    Ok(()) => {
                        entry.values.insert(field, value.clone());
                        entry.drafts.insert(field, value.clone());
                        self.set_status(
                            &format!(
                                "{} · {} set to {}",
                                agent_name,
                                field,
                                if value.is_empty() { "default" } else { &value }
                            ),
                            true,
                        );
                    }
                    Err(error) => {
                        self.set_status(&format!("Could not update setting: {error:#}"), false)
                    }
                }
            }
            Action::Reload => self.reload_settings(),
            Action::Sync => self.start_catalog_sync(true),
            Action::Quit => {
                self.exiting = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    fn reload_settings(&mut self) {
        let mut failure = None;
        for row in &mut self.rows {
            match values_for(&row.agent) {
                Ok(values) => {
                    row.drafts.clone_from(&values);
                    row.values = values;
                }
                Err(error) => failure = Some(format!("{}: {error:#}", row.agent.spec.name)),
            }
        }
        match failure {
            Some(error) => self.set_status(&format!("Could not reload settings: {error}"), false),
            None => self.set_status("Settings reloaded", true),
        }
    }

    fn start_catalog_sync(&mut self, force: bool) {
        if self.syncing {
            return;
        }
        let (sender, receiver) = mpsc::channel();
        let sync = async move {
            let result = (if force {
                catalog::sync_models_dev().await.map(Some)
            } else {
                catalog::sync_if_stale().await
            })
            .and_then(|synced| agent::sync_catalog_models().map(|()| synced))
            .map_err(|error| format!("{error:#}"));
            let _ = sender.send(result);
        };
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn(sync);
                self.sync_receiver = Some(receiver);
                self.syncing = true;
                self.set_status("Refreshing model catalog…", true);
            }
            Err(error) => self.set_status(
                &format!("Catalog refresh needs an async runtime: {error}"),
                false,
            ),
        }
    }

    fn receive_sync_result(&mut self) {
        let result = match self.sync_receiver.as_ref().map(Receiver::try_recv) {
            Some(Ok(result)) => Some(result),
            Some(Err(TryRecvError::Disconnected)) => {
                Some(Err("catalog refresh worker stopped".to_owned()))
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
                self.model_choices = model_choices().unwrap_or_default();
                self.reload_settings();
                self.set_status(
                    &format!("Model catalog refreshed for {count} providers"),
                    true,
                );
            }
            Ok(None) => self.set_status("Model catalog is up to date", true),
            Err(error) => self.set_status(&format!("Catalog refresh failed: {error}"), false),
        }
    }

    fn set_status(&mut self, message: &str, ok: bool) {
        self.status = message.to_owned();
        self.status_ok = ok;
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.process_events(ctx);
        ctx.request_repaint_after(Duration::from_millis(250));
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        self.logic(ui.ctx(), frame);
        let mut actions = Vec::new();
        egui::Frame::default()
            .fill(ui.visuals().panel_fill)
            .show(ui, |ui| {
                ui.set_min_size(ui.available_size());
                self.render_header(ui, &mut actions);
                ui.add_space(12.0);
                ui.separator();
                ui.add_space(12.0);

                let available = ui.available_size();
                let content_height = (available.y - 44.0).max(140.0);
                ui.horizontal(|ui| {
                    let mut selected = self.selected;
                    ui.allocate_ui_with_layout(
                        egui::vec2(228.0, content_height),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| self.render_sidebar(ui, &mut selected),
                    );
                    self.selected = selected;
                    ui.separator();
                    ui.allocate_ui_with_layout(
                        egui::vec2((available.x - 248.0).max(300.0), content_height),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| self.render_agent(ui, &mut actions),
                    );
                });

                ui.separator();
                ui.horizontal(|ui| {
                    let color = if self.status_ok {
                        ui.visuals().weak_text_color()
                    } else {
                        egui::Color32::from_rgb(225, 105, 105)
                    };
                    ui.label(RichText::new(&self.status).small().color(color));
                    if self.syncing {
                        ui.spinner();
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(RichText::new(format!("v{}", crate::VERSION)).small());
                    });
                });
            });
        for action in actions {
            self.apply_action(action, ui.ctx());
        }
    }
}

fn create_tray(ctx: &egui::Context, signals: &Signals) -> Result<TrayIcon> {
    let open = MenuItem::new("Open magpie", true, None);
    let separator = PredefinedMenuItem::separator();
    let quit = MenuItem::new("Quit", true, None);
    let menu = Menu::new();
    let items: [&dyn tray_icon::menu::IsMenuItem; 3] = [&open, &separator, &quit];
    menu.append_items(&items).context("create tray menu")?;

    let icon = tray_icon_from_png()?;
    let tray = TrayIconBuilder::new()
        .with_tooltip("magpie")
        .with_icon(icon)
        .with_menu(Box::new(menu))
        .build()
        .context("create system tray icon")?;

    let open_id = open.id().clone();
    let quit_id = quit.id().clone();
    let open_signal = Arc::clone(&signals.open);
    let quit_signal = Arc::clone(&signals.quit);
    let menu_ctx = ctx.clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        if event.id() == &open_id {
            open_signal.store(true, Ordering::Relaxed);
            menu_ctx.request_repaint();
        } else if event.id() == &quit_id {
            quit_signal.store(true, Ordering::Relaxed);
            menu_ctx.request_repaint();
        }
    }));

    let toggle_signal = Arc::clone(&signals.toggle);
    let tray_ctx = ctx.clone();
    TrayIconEvent::set_event_handler(Some(move |event| {
        if matches!(
            event,
            TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            }
        ) {
            toggle_signal.store(true, Ordering::Relaxed);
            tray_ctx.request_repaint();
        }
    }));
    Ok(tray)
}

fn tray_icon_from_png() -> Result<Icon> {
    Icon::from_rgba(icon_pixels()?, 32, 32).context("build tray icon")
}

fn window_icon_data() -> Result<egui::IconData> {
    Ok(egui::IconData {
        rgba: icon_pixels()?,
        width: 32,
        height: 32,
    })
}

fn icon_pixels() -> Result<Vec<u8>> {
    Ok(
        image::load_from_memory(include_bytes!("../internal/gui/icon.png"))
            .context("decode application icon")?
            .resize_exact(32, 32, image::imageops::FilterType::Lanczos3)
            .into_rgba8()
            .into_raw(),
    )
}

fn values_for(current: &agent::Agent) -> Result<BTreeMap<&'static str, String>> {
    current.values().map(|values| values.into_iter().collect())
}

fn model_choices() -> Result<Vec<ModelChoice>> {
    let entries = provider::available_model_entries()?;
    let ready_models = entries
        .iter()
        .map(|entry| entry.id.as_str())
        .collect::<HashSet<_>>();
    let mut choices = entries
        .iter()
        .map(|entry| ModelChoice {
            value: entry.id.clone(),
            label: if entry.model.name.is_empty() {
                entry.model.id.clone()
            } else {
                entry.model.name.clone()
            },
            provider: entry.provider_name.clone(),
        })
        .collect::<Vec<_>>();
    for group in provider::groups()?.into_iter().filter(|group| {
        !group.hidden
            && group
                .members
                .iter()
                .any(|member| ready_models.contains(member.as_str()))
    }) {
        choices.push(ModelChoice {
            value: format!("group/{}", group.id),
            label: group.name.clone(),
            provider: "routing group".to_owned(),
        });
    }
    Ok(choices)
}
