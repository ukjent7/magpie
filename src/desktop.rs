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

use crate::{agent, catalog, profile, provider, settings};

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

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Agents,
    Providers,
    Profiles,
    Groups,
}

#[derive(Clone, Default)]
struct ProviderDraft {
    source: ProviderSource,
    preset_id: String,
    name: String,
    endpoint: String,
    key: String,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum ProviderSource {
    #[default]
    Preset,
    Custom,
}

enum Action {
    SetField {
        row: usize,
        field: &'static str,
        value: String,
    },
    Reload,
    Sync,
    OpenProviderForm,
    SelectProvider(usize),
    AddProvider(ProviderDraft),
    SetProviderKey {
        id: String,
        key: String,
    },
    ConfirmProviderRemoval(String),
    RemoveProvider(String),
    RefreshProvider(String),
    SelectProfile(usize),
    OpenProfileForm,
    SaveProfile(String),
    ConfirmProfileApply(String),
    ApplyProfile(String),
    ConfirmProfileRemoval(String),
    RemoveProfile(String),
    SelectGroup(usize),
    CreateGroup,
    EditGroup(usize),
    SaveGroup(provider::Group),
    ConfirmGroupRemoval(String),
    RemoveGroup(String),
    RestoreGroup(String),
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
    page: Page,
    providers: Vec<provider::DesktopProvider>,
    presets: Vec<provider::DesktopPreset>,
    selected_provider: usize,
    provider_draft: ProviderDraft,
    provider_form_open: bool,
    provider_key_for: String,
    provider_key_draft: String,
    confirm_remove: Option<String>,
    provider_refreshing: bool,
    provider_receiver: Option<Receiver<(String, std::result::Result<usize, String>)>>,
    profiles: Vec<(String, String)>,
    selected_profile: usize,
    profile_form_open: bool,
    profile_name_draft: String,
    confirm_profile_apply: Option<String>,
    confirm_profile_removal: Option<String>,
    groups: Vec<provider::Group>,
    group_models: Vec<provider::ModelEntry>,
    selected_group: usize,
    group_form_open: bool,
    group_draft: Option<provider::Group>,
    confirm_group_removal: Option<String>,
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
        let presets = provider::desktop_presets();
        let provider_draft = ProviderDraft {
            preset_id: presets
                .first()
                .map_or_else(String::new, |preset| preset.id.clone()),
            ..ProviderDraft::default()
        };
        let providers = match provider::desktop_providers() {
            Ok(providers) => providers,
            Err(error) => {
                status = format!("Could not load providers: {error:#}");
                status_ok = false;
                Vec::new()
            }
        };
        let profiles = match profile::list_entries() {
            Ok(profiles) => profiles,
            Err(error) => {
                status = format!("Could not load profiles: {error:#}");
                status_ok = false;
                Vec::new()
            }
        };
        let (groups, group_models) = match provider::desktop_group_data() {
            Ok(data) => data,
            Err(error) => {
                status = format!("Could not load routing groups: {error:#}");
                status_ok = false;
                (Vec::new(), Vec::new())
            }
        };
        let mut app = Self {
            rows,
            selected: 0,
            page: Page::Agents,
            providers,
            presets,
            selected_provider: 0,
            provider_draft,
            provider_form_open: false,
            provider_key_for: String::new(),
            provider_key_draft: String::new(),
            confirm_remove: None,
            provider_refreshing: false,
            provider_receiver: None,
            profiles,
            selected_profile: 0,
            profile_form_open: false,
            profile_name_draft: String::new(),
            confirm_profile_apply: None,
            confirm_profile_removal: None,
            groups,
            group_models,
            selected_group: 0,
            group_form_open: false,
            group_draft: None,
            confirm_group_removal: None,
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
        self.receive_provider_result();
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
            ui.add_space(18.0);
            if ui
                .selectable_label(self.page == Page::Agents, "Agents")
                .clicked()
            {
                self.page = Page::Agents;
            }
            if ui
                .selectable_label(self.page == Page::Providers, "Providers")
                .clicked()
            {
                self.page = Page::Providers;
            }
            if ui
                .selectable_label(self.page == Page::Profiles, "Profiles")
                .clicked()
            {
                self.page = Page::Profiles;
            }
            if ui
                .selectable_label(self.page == Page::Groups, "Groups")
                .clicked()
            {
                self.page = Page::Groups;
            }
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

    fn render_provider_sidebar(&self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        ui.horizontal(|ui| {
            ui.heading("Providers");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .small_button("+")
                    .on_hover_text("Add a provider")
                    .clicked()
                {
                    actions.push(Action::OpenProviderForm);
                }
            });
        });
        ui.add_space(8.0);
        if self.providers.is_empty() {
            ui.label("No providers configured.");
            ui.add_space(8.0);
            ui.label("Add a preset or connect an OpenAI compatible API.");
            return;
        }
        egui::ScrollArea::vertical().show(ui, |ui| {
            for (index, provider) in self.providers.iter().enumerate() {
                let credential = if provider.account {
                    "signed in"
                } else if provider.has_key {
                    "key set"
                } else if provider.key_required {
                    "key needed"
                } else {
                    "local"
                };
                let label = format!(
                    "{}\n{} models · {credential}",
                    provider.name, provider.model_count
                );
                if ui
                    .selectable_label(self.selected_provider == index, label)
                    .clicked()
                {
                    actions.push(Action::SelectProvider(index));
                }
            }
        });
    }

    fn render_profile_sidebar(&self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        ui.horizontal(|ui| {
            ui.heading("Profiles");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .small_button("+")
                    .on_hover_text("Save current agent settings as a profile")
                    .clicked()
                {
                    actions.push(Action::OpenProfileForm);
                }
            });
        });
        ui.add_space(8.0);
        if self.profiles.is_empty() {
            ui.label("No profiles saved yet.");
            ui.add_space(8.0);
            if ui.button("Save current settings").clicked() {
                actions.push(Action::OpenProfileForm);
            }
            return;
        }
        egui::ScrollArea::vertical().show(ui, |ui| {
            for (index, (name, description)) in self.profiles.iter().enumerate() {
                if ui
                    .selectable_label(
                        self.selected_profile == index,
                        format!("{name}\n{description}"),
                    )
                    .clicked()
                {
                    actions.push(Action::SelectProfile(index));
                }
            }
        });
    }

    fn render_group_sidebar(&self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        ui.horizontal(|ui| {
            ui.heading("Groups");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .small_button("+")
                    .on_hover_text("Create a routing group")
                    .clicked()
                {
                    actions.push(Action::CreateGroup);
                }
            });
        });
        ui.add_space(8.0);
        if self.groups.is_empty() {
            ui.label("No routing groups yet.");
            ui.add_space(8.0);
            if ui.button("Create a group").clicked() {
                actions.push(Action::CreateGroup);
            }
            return;
        }
        egui::ScrollArea::vertical().show(ui, |ui| {
            for (index, group) in self.groups.iter().enumerate() {
                let state = if group.hidden {
                    "removed"
                } else if group.auto {
                    "discovered"
                } else {
                    "custom"
                };
                let label = format!("{}\n{} models · {state}", group.name, group.members.len());
                if ui
                    .selectable_label(self.selected_group == index, label)
                    .clicked()
                {
                    actions.push(Action::SelectGroup(index));
                }
            }
        });
    }

    fn render_provider_details(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        let Some(provider) = self.providers.get(self.selected_provider).cloned() else {
            ui.vertical_centered(|ui| {
                ui.add_space(80.0);
                ui.heading("Connect a provider");
                ui.label("Choose a preset or add your own API endpoint.");
                if ui.button("Add provider").clicked() {
                    actions.push(Action::OpenProviderForm);
                }
            });
            return;
        };

        if self.provider_key_for != provider.id {
            self.provider_key_for.clone_from(&provider.id);
            self.provider_key_draft.clear();
        }

        ui.horizontal(|ui| {
            ui.heading(&provider.name);
            if provider.hidden {
                ui.label(
                    RichText::new("Hidden")
                        .small()
                        .color(ui.visuals().weak_text_color()),
                );
            }
        });
        ui.label(
            RichText::new(&provider.id)
                .small()
                .color(ui.visuals().weak_text_color()),
        );
        ui.add_space(8.0);
        ui.label("Endpoint");
        ui.label(RichText::new(&provider.endpoint).monospace());
        ui.add_space(8.0);
        ui.horizontal_wrapped(|ui| {
            for protocol in &provider.protocols {
                ui.label(
                    RichText::new(protocol)
                        .small()
                        .background_color(ui.visuals().code_bg_color),
                );
            }
        });
        ui.add_space(12.0);
        ui.horizontal_wrapped(|ui| {
            ui.label(format!("{} models", provider.model_count));
            ui.separator();
            if provider.account {
                ui.label("Signed-in account");
            } else {
                ui.label(format!("{} active key(s)", provider.active_keys));
            }
            ui.separator();
            ui.label(format!("key routing · {}", provider.routing));
            ui.separator();
            ui.label(format!("affinity · {}", provider.affinity));
        });
        if !provider.models.is_empty() {
            ui.add_space(8.0);
            let preview = provider.models.join(" · ");
            let text = if provider.model_count > provider.models.len() {
                format!(
                    "{preview} · +{} more",
                    provider.model_count - provider.models.len()
                )
            } else {
                preview
            };
            ui.label(RichText::new(text).small());
        }

        ui.add_space(18.0);
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!self.provider_refreshing, egui::Button::new("Fetch models"))
                .clicked()
            {
                actions.push(Action::RefreshProvider(provider.id.clone()));
            }
            if !provider.account && ui.button("Remove provider").clicked() {
                actions.push(Action::ConfirmProviderRemoval(provider.id.clone()));
            }
        });

        if provider.key_required && !provider.account {
            ui.add_space(18.0);
            ui.separator();
            ui.heading("API key");
            ui.horizontal(|ui| {
                let width = ui.available_width().min(360.0);
                ui.add_sized(
                    [width, 30.0],
                    egui::TextEdit::singleline(&mut self.provider_key_draft)
                        .password(true)
                        .hint_text(if provider.has_key {
                            "Enter a replacement key"
                        } else {
                            "Enter the API key"
                        }),
                );
                if ui
                    .add_enabled(
                        !self.provider_key_draft.trim().is_empty(),
                        egui::Button::new("Save key"),
                    )
                    .clicked()
                {
                    actions.push(Action::SetProviderKey {
                        id: provider.id.clone(),
                        key: self.provider_key_draft.clone(),
                    });
                }
            });
            ui.label(
                RichText::new(if provider.has_key {
                    "The current key remains private and is never shown again."
                } else {
                    "The key is stored in your local Magpie provider settings."
                })
                .small()
                .color(ui.visuals().weak_text_color()),
            );
        }
    }

    fn render_profile_details(&self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        let Some((name, description)) = self.profiles.get(self.selected_profile) else {
            ui.vertical_centered(|ui| {
                ui.add_space(80.0);
                ui.heading("Save an agent profile");
                ui.label("Profiles keep reusable model and behavior settings for your agents.");
                if ui.button("Save current settings").clicked() {
                    actions.push(Action::OpenProfileForm);
                }
            });
            return;
        };

        ui.heading(name);
        ui.label(
            RichText::new(description)
                .small()
                .color(ui.visuals().weak_text_color()),
        );
        ui.add_space(12.0);
        ui.label("A profile can set model and behavior fields for detected agents.");
        ui.label("Settings not included in the profile keep their current values.");
        ui.add_space(20.0);
        ui.horizontal(|ui| {
            if ui.button("Apply profile").clicked() {
                actions.push(Action::ConfirmProfileApply(name.clone()));
            }
            if ui.button("Delete profile").clicked() {
                actions.push(Action::ConfirmProfileRemoval(name.clone()));
            }
        });
    }

    fn render_group_details(&self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        let Some(group) = self.groups.get(self.selected_group) else {
            ui.vertical_centered(|ui| {
                ui.add_space(80.0);
                ui.heading("Create a routing group");
                ui.label("Combine provider models behind one gateway model name.");
                if ui.button("Create group").clicked() {
                    actions.push(Action::CreateGroup);
                }
            });
            return;
        };

        ui.horizontal(|ui| {
            ui.heading(&group.name);
            if group.auto {
                ui.label(
                    RichText::new("Discovered")
                        .small()
                        .color(ui.visuals().weak_text_color()),
                );
            }
            if group.hidden {
                ui.label(
                    RichText::new("Removed")
                        .small()
                        .color(ui.visuals().weak_text_color()),
                );
            }
        });
        ui.label(
            RichText::new(format!("group/{}", group.id))
                .small()
                .monospace()
                .color(ui.visuals().weak_text_color()),
        );
        ui.add_space(8.0);
        ui.horizontal_wrapped(|ui| {
            ui.label(format!("Routing · {}", group_routing_name(&group.routing)));
            ui.separator();
            ui.label(format!(
                "Affinity · {}",
                group_affinity_name(&group.affinity)
            ));
            ui.separator();
            ui.label(format!("{} models", group.members.len()));
        });
        if group.auto && !group.hidden {
            ui.label(
                RichText::new("Saving edits turns this discovered group into a custom group.")
                    .small()
                    .color(ui.visuals().weak_text_color()),
            );
        }
        ui.add_space(10.0);
        egui::ScrollArea::vertical().show(ui, |ui| {
            for member in &group.members {
                let label = self
                    .group_models
                    .iter()
                    .find(|entry| entry.id == *member)
                    .map_or_else(
                        || format!("{member}  ·  currently unavailable"),
                        |entry| {
                            let model_name = if entry.model.name.is_empty() {
                                &entry.model.id
                            } else {
                                &entry.model.name
                            };
                            format!("{}  ·  {model_name}", entry.provider_name)
                        },
                    );
                ui.label(label);
            }
        });
        ui.add_space(14.0);
        ui.horizontal(|ui| {
            if group.hidden {
                if ui.button("Restore group").clicked() {
                    actions.push(Action::RestoreGroup(group.id.clone()));
                }
            } else {
                if ui.button("Edit group").clicked() {
                    actions.push(Action::EditGroup(self.selected_group));
                }
                if ui.button("Remove group").clicked() {
                    actions.push(Action::ConfirmGroupRemoval(group.id.clone()));
                }
            }
        });
    }

    fn render_provider_form(&mut self, ctx: &egui::Context, actions: &mut Vec<Action>) {
        if !self.provider_form_open {
            return;
        }
        let mut open = self.provider_form_open;
        let mut should_close = false;
        egui::Window::new("Add provider")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.radio_value(
                        &mut self.provider_draft.source,
                        ProviderSource::Preset,
                        "Preset",
                    );
                    ui.radio_value(
                        &mut self.provider_draft.source,
                        ProviderSource::Custom,
                        "Custom endpoint",
                    );
                });
                ui.add_space(8.0);

                if self.provider_draft.source == ProviderSource::Custom {
                    ui.label("Provider name");
                    ui.text_edit_singleline(&mut self.provider_draft.name);
                    ui.add_space(6.0);
                    ui.label("OpenAI compatible base URL");
                    ui.text_edit_singleline(&mut self.provider_draft.endpoint);
                } else {
                    let selected = self
                        .presets
                        .iter()
                        .find(|preset| preset.id == self.provider_draft.preset_id);
                    egui::ComboBox::from_id_salt("provider-preset")
                        .selected_text(
                            selected.map_or("Choose a preset", |preset| preset.name.as_str()),
                        )
                        .show_ui(ui, |ui| {
                            for preset in &self.presets {
                                ui.selectable_value(
                                    &mut self.provider_draft.preset_id,
                                    preset.id.clone(),
                                    preset.name.as_str(),
                                );
                            }
                        });
                    if let Some(preset) = selected {
                        ui.label(RichText::new(&preset.endpoint).small());
                    }
                }

                let selected = self
                    .presets
                    .iter()
                    .find(|preset| preset.id == self.provider_draft.preset_id);
                let key_required = selected.is_some_and(|preset| preset.key_required);
                let custom = self.provider_draft.source == ProviderSource::Custom;
                let show_key = custom || key_required;
                if show_key {
                    ui.add_space(6.0);
                    ui.label("API key");
                    ui.add(egui::TextEdit::singleline(&mut self.provider_draft.key).password(true));
                } else if !custom {
                    ui.label("This local provider usually does not need an API key.");
                }

                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    let valid = if custom {
                        !self.provider_draft.name.trim().is_empty()
                            && !self.provider_draft.endpoint.trim().is_empty()
                    } else {
                        selected.is_some()
                            && (!key_required || !self.provider_draft.key.trim().is_empty())
                    };
                    if ui
                        .add_enabled(valid, egui::Button::new("Add provider"))
                        .clicked()
                    {
                        actions.push(Action::AddProvider(self.provider_draft.clone()));
                    }
                    if ui.button("Cancel").clicked() {
                        should_close = true;
                    }
                });
            });
        if should_close {
            open = false;
        }
        self.provider_form_open = open;
        if !open {
            self.provider_draft.key.clear();
        }
    }

    fn render_remove_confirmation(&mut self, ctx: &egui::Context, actions: &mut Vec<Action>) {
        let Some(id) = self.confirm_remove.clone() else {
            return;
        };
        let name = self
            .providers
            .iter()
            .find(|provider| provider.id == id)
            .map_or(id.as_str(), |provider| provider.name.as_str())
            .to_owned();
        let mut open = true;
        egui::Window::new("Remove provider?")
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(format!("Remove {name} and its saved API keys?"));
                ui.horizontal(|ui| {
                    if ui.button("Remove").clicked() {
                        actions.push(Action::RemoveProvider(id.clone()));
                    }
                    if ui.button("Cancel").clicked() {
                        self.confirm_remove = None;
                    }
                });
            });
        if !open {
            self.confirm_remove = None;
        }
    }

    fn render_profile_dialogs(&mut self, ctx: &egui::Context, actions: &mut Vec<Action>) {
        if self.profile_form_open {
            let mut open = true;
            let mut should_close = false;
            egui::Window::new("Save profile")
                .collapsible(false)
                .resizable(false)
                .open(&mut open)
                .show(ctx, |ui| {
                    ui.label("Save the current settings from detected agents.");
                    ui.add_space(8.0);
                    ui.label("Profile name");
                    ui.text_edit_singleline(&mut self.profile_name_draft);
                    let name = self.profile_name_draft.trim();
                    let replaces =
                        !name.is_empty() && self.profiles.iter().any(|(saved, _)| saved == name);
                    if replaces {
                        ui.label("Saving will update the profile with this name.");
                    }
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(!name.is_empty(), egui::Button::new("Save profile"))
                            .clicked()
                        {
                            actions.push(Action::SaveProfile(name.to_owned()));
                        }
                        if ui.button("Cancel").clicked() {
                            should_close = true;
                        }
                    });
                });
            if should_close {
                open = false;
            }
            self.profile_form_open = open;
            if !open {
                self.profile_name_draft.clear();
            }
        }

        if let Some(name) = self.confirm_profile_apply.clone() {
            let mut open = true;
            let mut should_close = false;
            egui::Window::new("Apply profile?")
                .collapsible(false)
                .resizable(false)
                .open(&mut open)
                .show(ctx, |ui| {
                    ui.label(format!("Apply {name} to the detected agent settings?"));
                    ui.label("This updates the agents' local configuration files.");
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        if ui.button("Apply profile").clicked() {
                            actions.push(Action::ApplyProfile(name.clone()));
                        }
                        if ui.button("Cancel").clicked() {
                            should_close = true;
                        }
                    });
                });
            if !open || should_close {
                self.confirm_profile_apply = None;
            }
        }

        if let Some(name) = self.confirm_profile_removal.clone() {
            let mut open = true;
            let mut should_close = false;
            egui::Window::new("Delete profile?")
                .collapsible(false)
                .resizable(false)
                .open(&mut open)
                .show(ctx, |ui| {
                    ui.label(format!("Delete the saved profile {name}?"));
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        if ui.button("Delete profile").clicked() {
                            actions.push(Action::RemoveProfile(name.clone()));
                        }
                        if ui.button("Cancel").clicked() {
                            should_close = true;
                        }
                    });
                });
            if !open || should_close {
                self.confirm_profile_removal = None;
            }
        }
    }

    fn render_group_form(&mut self, ctx: &egui::Context, actions: &mut Vec<Action>) {
        if !self.group_form_open {
            return;
        }
        let Some(draft) = self.group_draft.as_mut() else {
            self.group_form_open = false;
            return;
        };
        let title = if draft.id.is_empty() {
            "New routing group"
        } else {
            "Edit routing group"
        };
        let models = &self.group_models;
        let mut open = true;
        let mut should_close = false;
        egui::Window::new(title)
            .default_width(620.0)
            .collapsible(false)
            .resizable(true)
            .open(&mut open)
            .show(ctx, |ui| {
                if !draft.id.is_empty() {
                    ui.label(
                        RichText::new(format!("group/{}", draft.id))
                            .small()
                            .monospace()
                            .color(ui.visuals().weak_text_color()),
                    );
                }
                if draft.auto {
                    ui.label("Saving edits makes this discovered group a custom group.");
                }
                ui.label("Group name");
                ui.text_edit_singleline(&mut draft.name);
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.label("Routing");
                    egui::ComboBox::from_id_salt("group-routing")
                        .selected_text(group_routing_name(&draft.routing))
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut draft.routing, String::new(), "Smart");
                            ui.selectable_value(&mut draft.routing, "order".to_owned(), "Order");
                            ui.selectable_value(&mut draft.routing, "rotate".to_owned(), "Rotate");
                            ui.selectable_value(
                                &mut draft.routing,
                                "usage".to_owned(),
                                "Least used",
                            );
                        });
                    ui.add_space(12.0);
                    ui.label("Affinity");
                    egui::ComboBox::from_id_salt("group-affinity")
                        .selected_text(group_affinity_name(&draft.affinity))
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut draft.affinity, String::new(), "Auto");
                            ui.selectable_value(
                                &mut draft.affinity,
                                "session".to_owned(),
                                "Session",
                            );
                            ui.selectable_value(&mut draft.affinity, "turn".to_owned(), "Turn");
                            ui.selectable_value(&mut draft.affinity, "off".to_owned(), "Off");
                        });
                });

                ui.add_space(10.0);
                ui.label(format!("Models · {} selected", draft.members.len()));
                if models.is_empty() {
                    ui.label("No available models. Add a provider and fetch its models first.");
                } else {
                    egui::ScrollArea::vertical()
                        .max_height(300.0)
                        .show(ui, |ui| {
                            for entry in models {
                                let model_name = if entry.model.name.is_empty() {
                                    &entry.model.id
                                } else {
                                    &entry.model.name
                                };
                                let label = format!(
                                    "{}  ·  {}  ·  {}",
                                    entry.provider_name, model_name, entry.id
                                );
                                let mut selected = draft.members.contains(&entry.id);
                                let changed = ui
                                    .push_id(&entry.id, |ui| ui.checkbox(&mut selected, label))
                                    .inner
                                    .changed();
                                if changed && selected {
                                    draft.members.push(entry.id.clone());
                                } else if changed {
                                    draft.members.retain(|member| member != &entry.id);
                                }
                            }
                        });
                }

                let unavailable = draft
                    .members
                    .iter()
                    .filter(|member| !models.iter().any(|entry| &entry.id == *member))
                    .cloned()
                    .collect::<Vec<_>>();
                if !unavailable.is_empty() {
                    ui.add_space(6.0);
                    ui.label("Unavailable models are skipped until a provider serves them again:");
                    for member in unavailable {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(&member).small().monospace());
                            if ui.small_button("Remove").clicked() {
                                draft.members.retain(|saved| saved != &member);
                            }
                        });
                    }
                }

                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    let valid = !draft.name.trim().is_empty() && !draft.members.is_empty();
                    if ui
                        .add_enabled(valid, egui::Button::new("Save group"))
                        .clicked()
                    {
                        actions.push(Action::SaveGroup(draft.clone()));
                    }
                    if ui.button("Cancel").clicked() {
                        should_close = true;
                    }
                });
            });
        if should_close || !open {
            self.group_form_open = false;
            self.group_draft = None;
        }
    }

    fn render_group_removal_confirmation(
        &mut self,
        ctx: &egui::Context,
        actions: &mut Vec<Action>,
    ) {
        let Some(id) = self.confirm_group_removal.clone() else {
            return;
        };
        let name = self
            .groups
            .iter()
            .find(|group| group.id == id)
            .map_or(id.as_str(), |group| group.name.as_str())
            .to_owned();
        let mut open = true;
        let mut should_close = false;
        egui::Window::new("Remove routing group?")
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(format!("Remove {name} from the gateway?"));
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if ui.button("Remove group").clicked() {
                        actions.push(Action::RemoveGroup(id.clone()));
                    }
                    if ui.button("Cancel").clicked() {
                        should_close = true;
                    }
                });
            });
        if should_close || !open {
            self.confirm_group_removal = None;
        }
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
            Action::Reload => {
                self.reload_settings();
                if let Err(error) = self.reload_provider_data() {
                    self.set_status(&format!("Could not reload providers: {error:#}"), false);
                }
                if let Err(error) = self.reload_profile_data() {
                    self.set_status(&format!("Could not reload profiles: {error:#}"), false);
                }
            }
            Action::Sync => self.start_catalog_sync(true),
            Action::OpenProviderForm => {
                self.provider_draft.key.clear();
                self.provider_form_open = true;
            }
            Action::SelectProvider(index) => {
                self.selected_provider = index.min(self.providers.len().saturating_sub(1));
                self.provider_key_draft.clear();
            }
            Action::AddProvider(draft) => {
                let added = if draft.source == ProviderSource::Custom {
                    provider::add_desktop_custom(&draft.name, &draft.endpoint, &draft.key)
                } else {
                    provider::add_desktop_preset(&draft.preset_id, &draft.key)
                };
                match added {
                    Ok(id) => match self.reload_provider_data() {
                        Ok(()) => {
                            self.selected_provider = self
                                .providers
                                .iter()
                                .position(|provider| provider.id == id)
                                .unwrap_or(self.selected_provider);
                            self.provider_form_open = false;
                            self.provider_draft.key.clear();
                            self.set_status(&format!("Added provider {id}"), true);
                        }
                        Err(error) => self.set_status(
                            &format!("Provider added, but the list could not reload: {error:#}"),
                            false,
                        ),
                    },
                    Err(error) => {
                        self.set_status(&format!("Could not add provider: {error:#}"), false)
                    }
                }
            }
            Action::SetProviderKey { id, key } => match provider::set_desktop_key(&id, &key) {
                Ok(()) => {
                    self.provider_key_draft.clear();
                    match self.reload_provider_data() {
                        Ok(()) => self.set_status(&format!("Updated API key for {id}"), true),
                        Err(error) => self.set_status(
                            &format!(
                                "API key updated, but the provider list could not reload: {error:#}"
                            ),
                            false,
                        ),
                    }
                }
                Err(error) => self.set_status(&format!("Could not save API key: {error:#}"), false),
            },
            Action::ConfirmProviderRemoval(id) => self.confirm_remove = Some(id),
            Action::RemoveProvider(id) => match provider::remove_desktop_provider(&id) {
                Ok(()) => {
                    self.confirm_remove = None;
                    match self.reload_provider_data() {
                        Ok(()) => self.set_status(&format!("Removed provider {id}"), true),
                        Err(error) => self.set_status(
                            &format!("Provider removed, but the list could not reload: {error:#}"),
                            false,
                        ),
                    }
                }
                Err(error) => {
                    self.set_status(&format!("Could not remove provider: {error:#}"), false)
                }
            },
            Action::RefreshProvider(id) => self.start_provider_refresh(id),
            Action::SelectProfile(index) => {
                self.selected_profile = index.min(self.profiles.len().saturating_sub(1));
            }
            Action::OpenProfileForm => {
                self.profile_name_draft.clear();
                self.profile_form_open = true;
            }
            Action::SaveProfile(name) => match profile::save_named(&name) {
                Ok(()) => {
                    self.profile_form_open = false;
                    self.profile_name_draft.clear();
                    match self.reload_profile_data() {
                        Ok(()) => {
                            self.selected_profile = self
                                .profiles
                                .iter()
                                .position(|(saved, _)| saved == &name)
                                .unwrap_or(self.selected_profile);
                            self.set_status(&format!("Saved profile {name}"), true);
                        }
                        Err(error) => self.set_status(
                            &format!("Profile saved, but the list could not reload: {error:#}"),
                            false,
                        ),
                    }
                }
                Err(error) => self.set_status(&format!("Could not save profile: {error:#}"), false),
            },
            Action::ConfirmProfileApply(name) => self.confirm_profile_apply = Some(name),
            Action::ApplyProfile(name) => {
                self.confirm_profile_apply = None;
                match profile::apply_named(&name) {
                    Ok(changed) => match self.reload_agent_values() {
                        Ok(()) => self.set_status(
                            &format!("Applied profile {name} · {changed} settings changed"),
                            true,
                        ),
                        Err(error) => self.set_status(
                            &format!("Profile applied, but settings could not reload: {error:#}"),
                            false,
                        ),
                    },
                    Err(error) => self
                        .set_status(&format!("Could not apply profile {name}: {error:#}"), false),
                }
            }
            Action::ConfirmProfileRemoval(name) => self.confirm_profile_removal = Some(name),
            Action::RemoveProfile(name) => {
                self.confirm_profile_removal = None;
                match profile::delete_named(&name) {
                    Ok(()) => match self.reload_profile_data() {
                        Ok(()) => self.set_status(&format!("Deleted profile {name}"), true),
                        Err(error) => self.set_status(
                            &format!("Profile deleted, but the list could not reload: {error:#}"),
                            false,
                        ),
                    },
                    Err(error) => self.set_status(
                        &format!("Could not delete profile {name}: {error:#}"),
                        false,
                    ),
                }
            }
            Action::SelectGroup(index) => {
                self.selected_group = index.min(self.groups.len().saturating_sub(1));
            }
            Action::CreateGroup => {
                self.group_draft = Some(provider::Group::default());
                self.group_form_open = true;
            }
            Action::EditGroup(index) => {
                if let Some(group) = self.groups.get(index).filter(|group| !group.hidden) {
                    self.group_draft = Some(group.clone());
                    self.group_form_open = true;
                }
            }
            Action::SaveGroup(group) => {
                let id = group.id.clone();
                let saved = if id.is_empty() {
                    provider::add_desktop_group(group)
                } else {
                    provider::save_group(group).map(|()| id)
                };
                match saved {
                    Ok(id) => {
                        self.group_form_open = false;
                        self.group_draft = None;
                        match self.reload_group_data() {
                            Ok(()) => {
                                self.selected_group = self
                                    .groups
                                    .iter()
                                    .position(|group| group.id == id)
                                    .unwrap_or(self.selected_group);
                                self.set_status(&format!("Saved group {id}"), true);
                            }
                            Err(error) => self.set_status(
                                &format!("Group saved, but the list could not reload: {error:#}"),
                                false,
                            ),
                        }
                    }
                    Err(error) => {
                        self.set_status(&format!("Could not save group: {error:#}"), false)
                    }
                }
            }
            Action::ConfirmGroupRemoval(id) => self.confirm_group_removal = Some(id),
            Action::RemoveGroup(id) => {
                self.confirm_group_removal = None;
                match provider::delete_group(&id) {
                    Ok(()) => match self.reload_group_data() {
                        Ok(()) => {
                            self.selected_group = self
                                .groups
                                .iter()
                                .position(|group| group.id == id)
                                .unwrap_or_else(|| {
                                    self.selected_group.min(self.groups.len().saturating_sub(1))
                                });
                            self.set_status(&format!("Removed group {id}"), true);
                        }
                        Err(error) => self.set_status(
                            &format!("Group removed, but the list could not reload: {error:#}"),
                            false,
                        ),
                    },
                    Err(error) => {
                        self.set_status(&format!("Could not remove group {id}: {error:#}"), false)
                    }
                }
            }
            Action::RestoreGroup(id) => match provider::restore_group(&id) {
                Ok(()) => match self.reload_group_data() {
                    Ok(()) => {
                        self.selected_group = self
                            .groups
                            .iter()
                            .position(|group| group.id == id)
                            .unwrap_or(self.selected_group);
                        self.set_status(&format!("Restored group {id}"), true);
                    }
                    Err(error) => self.set_status(
                        &format!("Group restored, but the list could not reload: {error:#}"),
                        false,
                    ),
                },
                Err(error) => {
                    self.set_status(&format!("Could not restore group {id}: {error:#}"), false)
                }
            },
            Action::Quit => {
                self.exiting = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    fn reload_settings(&mut self) {
        match self.reload_agent_values() {
            Ok(()) => self.set_status("Settings reloaded", true),
            Err(error) => self.set_status(&format!("Could not reload settings: {error:#}"), false),
        }
    }

    fn reload_agent_values(&mut self) -> Result<()> {
        let mut failures = Vec::new();
        for row in &mut self.rows {
            match values_for(&row.agent) {
                Ok(values) => {
                    row.drafts.clone_from(&values);
                    row.values = values;
                }
                Err(error) => failures.push(format!("{}: {error:#}", row.agent.spec.name)),
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(anyhow::anyhow!(failures.join("; ")))
        }
    }

    fn reload_profile_data(&mut self) -> Result<()> {
        self.profiles = profile::list_entries()?;
        self.selected_profile = self
            .selected_profile
            .min(self.profiles.len().saturating_sub(1));
        Ok(())
    }

    fn reload_provider_data(&mut self) -> Result<()> {
        self.providers = provider::desktop_providers()?;
        self.selected_provider = self
            .selected_provider
            .min(self.providers.len().saturating_sub(1));
        self.reload_group_data()?;
        Ok(())
    }

    fn reload_group_data(&mut self) -> Result<()> {
        let (groups, models) = provider::desktop_group_data()?;
        self.model_choices = model_choices_for(&models, &groups);
        self.groups = groups;
        self.group_models = models;
        self.selected_group = self.selected_group.min(self.groups.len().saturating_sub(1));
        Ok(())
    }

    fn start_provider_refresh(&mut self, id: String) {
        if self.provider_refreshing {
            return;
        }
        let (sender, receiver) = mpsc::channel();
        let refresh_id = id.clone();
        let refresh = async move {
            let result = provider::refresh_desktop_models(&refresh_id)
                .await
                .map_err(|error| format!("{error:#}"));
            let _ = sender.send((refresh_id, result));
        };
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn(refresh);
                self.provider_receiver = Some(receiver);
                self.provider_refreshing = true;
                self.set_status(&format!("Fetching models for {id}…"), true);
            }
            Err(error) => self.set_status(
                &format!("Fetching models needs an async runtime: {error}"),
                false,
            ),
        }
    }

    fn receive_provider_result(&mut self) {
        let result = match self.provider_receiver.as_ref().map(Receiver::try_recv) {
            Some(Ok(result)) => Some(result),
            Some(Err(TryRecvError::Disconnected)) => Some((
                String::new(),
                Err("provider refresh worker stopped".to_owned()),
            )),
            Some(Err(TryRecvError::Empty)) | None => None,
        };
        let Some((id, result)) = result else {
            return;
        };
        self.provider_receiver = None;
        self.provider_refreshing = false;
        match result {
            Ok(count) => match self.reload_provider_data() {
                Ok(()) => self.set_status(&format!("Fetched {count} models from {id}"), true),
                Err(error) => self.set_status(
                    &format!("Fetched models, but provider list could not reload: {error:#}"),
                    false,
                ),
            },
            Err(error) => {
                self.set_status(&format!("Could not fetch models from {id}: {error}"), false)
            }
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
            Ok(Some(count)) => match self.reload_provider_data() {
                Ok(()) => {
                    self.reload_settings();
                    self.set_status(
                        &format!("Model catalog refreshed for {count} providers"),
                        true,
                    );
                }
                Err(error) => self.set_status(
                    &format!("Catalog refreshed, but provider data could not reload: {error:#}"),
                    false,
                ),
            },
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
                    let mut selected = match self.page {
                        Page::Agents => self.selected,
                        Page::Providers => self.selected_provider,
                        Page::Profiles => self.selected_profile,
                        Page::Groups => self.selected_group,
                    };
                    ui.allocate_ui_with_layout(
                        egui::vec2(228.0, content_height),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| match self.page {
                            Page::Agents => self.render_sidebar(ui, &mut selected),
                            Page::Providers => self.render_provider_sidebar(ui, &mut actions),
                            Page::Profiles => self.render_profile_sidebar(ui, &mut actions),
                            Page::Groups => self.render_group_sidebar(ui, &mut actions),
                        },
                    );
                    match self.page {
                        Page::Agents => self.selected = selected,
                        Page::Providers => self.selected_provider = selected,
                        Page::Profiles => self.selected_profile = selected,
                        Page::Groups => self.selected_group = selected,
                    }
                    ui.separator();
                    ui.allocate_ui_with_layout(
                        egui::vec2((available.x - 248.0).max(300.0), content_height),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| match self.page {
                            Page::Agents => self.render_agent(ui, &mut actions),
                            Page::Providers => self.render_provider_details(ui, &mut actions),
                            Page::Profiles => self.render_profile_details(ui, &mut actions),
                            Page::Groups => self.render_group_details(ui, &mut actions),
                        },
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
        self.render_provider_form(ui.ctx(), &mut actions);
        self.render_remove_confirmation(ui.ctx(), &mut actions);
        self.render_profile_dialogs(ui.ctx(), &mut actions);
        self.render_group_form(ui.ctx(), &mut actions);
        self.render_group_removal_confirmation(ui.ctx(), &mut actions);
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

fn group_routing_name(routing: &str) -> &'static str {
    match routing {
        "order" => "Order",
        "rotate" => "Rotate",
        "usage" => "Least used",
        _ => "Smart",
    }
}

fn group_affinity_name(affinity: &str) -> &'static str {
    match affinity {
        "session" => "Session",
        "turn" => "Turn",
        "off" => "Off",
        _ => "Auto",
    }
}

fn model_choices() -> Result<Vec<ModelChoice>> {
    let (groups, entries) = provider::desktop_group_data()?;
    Ok(model_choices_for(&entries, &groups))
}

fn model_choices_for(
    entries: &[provider::ModelEntry],
    groups: &[provider::Group],
) -> Vec<ModelChoice> {
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
    for group in groups.iter().filter(|group| {
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
    choices
}
