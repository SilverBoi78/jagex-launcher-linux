//! Launcher state and layout.
//!
//! Anything that touches the network — signing in, checking for client updates,
//! downloading — runs on a worker thread and reports back through [`Log`] and a channel,
//! so the window never blocks.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};

use crate::clients::{self, JxEnv};
use crate::log::Log;
use crate::store::{Config, Session};

/// The clients the launcher can start.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Client {
    RuneLite,
    Rs3,
    Osrs,
}

impl Client {
    const ALL: [Client; 3] = [Client::RuneLite, Client::Rs3, Client::Osrs];

    fn title(self) -> &'static str {
        match self {
            Client::RuneLite => "RuneLite",
            Client::Rs3 => "RuneScape 3",
            Client::Osrs => "Old School",
        }
    }

    fn subtitle(self) -> &'static str {
        match self {
            Client::RuneLite => "Old School, third-party client",
            Client::Rs3 => "Official native Linux client",
            Client::Osrs => "Official client, via Wine",
        }
    }
}

/// Messages from worker threads back to the UI.
enum Event {
    LoggedIn(Box<Session>),
    SessionRefreshed(Box<Session>),
    LoggedOut,
    Done,
}

pub struct App {
    session: Option<Session>,
    config: Config,
    log: Log,
    selected_character: Option<String>,
    /// One network operation at a time, so buttons can be disabled while it runs.
    busy: Arc<AtomicBool>,
    tx: Sender<Event>,
    rx: Receiver<Event>,
    show_settings: bool,
}

impl App {
    pub fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        let log = Log::new();

        let session = match Session::load() {
            Ok(session) => session,
            Err(e) => {
                log.error(e);
                None
            }
        };
        let config = Config::load().unwrap_or_else(|e| {
            log.error(e);
            Config::default()
        });

        let selected_character = session.as_ref().and_then(|s| {
            s.selected_character
                .clone()
                .filter(|name| s.account.character_names().contains(name))
                .or_else(|| s.account.character_names().first().cloned())
        });

        match &session {
            Some(_) => log.info("signed in — ready to play"),
            None => log.info("not signed in"),
        }

        let (tx, rx) = channel();
        Self {
            session,
            config,
            log,
            selected_character,
            busy: Arc::new(AtomicBool::new(false)),
            tx,
            rx,
            show_settings: false,
        }
    }

    fn is_busy(&self) -> bool {
        self.busy.load(Ordering::SeqCst)
    }

    /// Runs `task` on a worker thread, holding the busy flag for its duration.
    ///
    /// Returns without doing anything if an operation is already in flight, so a double
    /// click cannot start two downloads.
    fn spawn<F>(&self, ctx: &egui::Context, task: F)
    where
        F: FnOnce(&Log, &Sender<Event>) -> anyhow::Result<()> + Send + 'static,
    {
        if self
            .busy
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }

        let log = self.log.clone();
        let tx = self.tx.clone();
        let busy = self.busy.clone();
        let ctx = ctx.clone();

        std::thread::spawn(move || {
            if let Err(e) = task(&log, &tx) {
                log.error(format!("{e:#}"));
            }
            busy.store(false, Ordering::SeqCst);
            let _ = tx.send(Event::Done);
            ctx.request_repaint();
        });
    }

    fn sign_in(&mut self, ctx: &egui::Context) {
        self.spawn(ctx, |log, tx| {
            log.info("opening the sign-in window...");
            let session = run_login_window()?;
            session.save()?;
            log.info("signed in");
            let _ = tx.send(Event::LoggedIn(Box::new(session)));
            Ok(())
        });
    }

    fn sign_out(&mut self, ctx: &egui::Context) {
        self.spawn(ctx, |log, tx| {
            Session::clear()?;
            log.info("signed out");
            let _ = tx.send(Event::LoggedOut);
            Ok(())
        });
    }

    /// Re-reads the character list, which also proves the stored session is still valid.
    fn refresh_characters(&mut self, ctx: &egui::Context) {
        let Some(session) = self.session.clone() else {
            return;
        };
        self.spawn(ctx, move |log, tx| {
            let crate::store::AccountSession::Jagex { session_id, .. } = &session.account else {
                return Ok(()); // legacy accounts have no character list to refresh
            };
            let session_id = session_id.clone();

            let client = crate::login::http_client()?;
            match crate::auth::session::fetch_accounts(&client, &session_id) {
                Ok(accounts) => {
                    let mut session = session;
                    session.account = crate::store::AccountSession::Jagex {
                        session_id,
                        accounts,
                    };
                    session.save()?;
                    log.info("character list refreshed");
                    let _ = tx.send(Event::SessionRefreshed(Box::new(session)));
                }
                Err(e) => {
                    // A lapsed session is not an error to shrug at — it means the stored
                    // credentials are useless and the user has to sign in again.
                    log.error(format!("{e:#}"));
                    Session::clear()?;
                    let _ = tx.send(Event::LoggedOut);
                }
            }
            Ok(())
        });
    }

    fn launch(&mut self, ctx: &egui::Context, which: Client) {
        let (Some(session), Some(character)) =
            (self.session.clone(), self.selected_character.clone())
        else {
            self.log.error("sign in and pick a character first");
            return;
        };
        let config = self.config.clone();
        let close_after = config.close_after_launch;
        let ctx_for_close = ctx.clone();

        self.spawn(ctx, move |log, tx| {
            let client = crate::login::http_client()?;

            // A legacy account hands its access token to the game, so it has to be fresh.
            let mut session = session;
            if clients::refresh_tokens_if_needed(&mut session, &client, log)? {
                let _ = tx.send(Event::SessionRefreshed(Box::new(session.clone())));
            }

            let jx = JxEnv::for_character(&session, &character)?;

            let (launch, wrapper) = match which {
                Client::RuneLite => (
                    clients::runelite::prepare(
                        &client,
                        config.runelite_custom_jar.as_deref(),
                        false,
                        log,
                    )?,
                    config.runelite_launch_command.clone(),
                ),
                Client::Rs3 => (
                    clients::rs3::prepare(&client, config.rs3_config_uri.as_deref(), log)?,
                    config.rs3_launch_command.clone(),
                ),
                Client::Osrs => (
                    clients::osrs::prepare(&client, log)?,
                    config.osrs_launch_command.clone(),
                ),
            };

            clients::spawn(launch, &jx, wrapper.as_deref(), log)?;

            if close_after {
                ctx_for_close.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            Ok(())
        });

        // Remember the choice for next time.
        if let Some(session) = &mut self.session {
            session.selected_character = self.selected_character.clone();
            if let Err(e) = session.save() {
                self.log.error(e);
            }
        }
    }

    fn drain_events(&mut self) {
        while let Ok(event) = self.rx.try_recv() {
            match event {
                Event::LoggedIn(session) | Event::SessionRefreshed(session) => {
                    let names = session.account.character_names();
                    // Keep the current pick if it still exists, else fall back to the first.
                    if !self
                        .selected_character
                        .as_ref()
                        .is_some_and(|c| names.contains(c))
                    {
                        self.selected_character = names.first().cloned();
                    }
                    self.session = Some(*session);
                }
                Event::LoggedOut => {
                    self.session = None;
                    self.selected_character = None;
                }
                Event::Done => {}
            }
        }
    }

    fn save_config(&self) {
        if let Err(e) = self.config.save() {
            self.log.error(e);
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.drain_events();

        let ctx = ui.ctx().clone();

        // Downloads update the log from a worker thread; keep repainting while they run.
        if self.is_busy() {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }

        egui::Panel::top("account").show(ui, |ui| {
            ui.add_space(6.0);
            self.account_bar(ui, &ctx);
            ui.add_space(6.0);
        });

        egui::Panel::bottom("log")
            .resizable(true)
            .default_size(160.0)
            .show(ui, |ui| self.log_pane(ui));

        egui::CentralPanel::default().show(ui, |ui| {
            if self.show_settings {
                self.settings_pane(ui);
            } else {
                self.client_tiles(ui, &ctx);
            }
        });
    }
}

impl App {
    fn account_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.horizontal(|ui| match &self.session {
            Some(session) => {
                let names = session.account.character_names();
                if let Some(account_name) = &session.account_name {
                    ui.strong(account_name.clone());
                    ui.separator();
                }
                ui.label("Character:");

                let selected = self
                    .selected_character
                    .clone()
                    .unwrap_or_else(|| "—".to_string());
                egui::ComboBox::from_id_salt("character")
                    .selected_text(selected)
                    .show_ui(ui, |ui| {
                        for name in &names {
                            ui.selectable_value(
                                &mut self.selected_character,
                                Some(name.clone()),
                                name,
                            );
                        }
                    });

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add_enabled(!self.is_busy(), egui::Button::new("Sign out"))
                        .clicked()
                    {
                        self.sign_out(ctx);
                    }
                    if ui
                        .add_enabled(!self.is_busy(), egui::Button::new("Refresh"))
                        .on_hover_text("Re-read the character list and check the session")
                        .clicked()
                    {
                        self.refresh_characters(ctx);
                    }
                    ui.toggle_value(&mut self.show_settings, "Settings");
                });
            }
            None => {
                ui.label("Not signed in");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add_enabled(
                            !self.is_busy(),
                            egui::Button::new("Sign in with a Jagex account"),
                        )
                        .clicked()
                    {
                        self.sign_in(ctx);
                    }
                    ui.toggle_value(&mut self.show_settings, "Settings");
                });
            }
        });
    }

    fn client_tiles(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let ready = self.session.is_some() && self.selected_character.is_some() && !self.is_busy();

        ui.add_space(8.0);
        for which in Client::ALL {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.heading(which.title());
                        ui.small(which.subtitle());
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add_enabled(
                                ready,
                                egui::Button::new("  Play  ").min_size(egui::vec2(80.0, 32.0)),
                            )
                            .clicked()
                        {
                            self.launch(ctx, which);
                        }
                    });
                });
            });
            ui.add_space(8.0);
        }

        if self.session.is_none() {
            ui.label("Sign in to play.");
        }
    }

    fn settings_pane(&mut self, ui: &mut egui::Ui) {
        let mut changed = false;

        ui.add_space(6.0);
        ui.heading("Settings");
        ui.add_space(6.0);

        changed |= ui
            .checkbox(
                &mut self.config.close_after_launch,
                "Close the launcher after starting a game",
            )
            .changed();

        ui.add_space(10.0);
        ui.label("RuneLite jar (leave empty to download automatically)");
        changed |= optional_text(ui, "runelite_jar", &mut self.config.runelite_custom_jar);

        ui.add_space(10.0);
        ui.label("RS3 config URI (leave empty for the default)");
        changed |= optional_text(ui, "rs3_config", &mut self.config.rs3_config_uri);

        ui.add_space(10.0);
        ui.label("Launch command wrappers — use %command% for the real command");
        for (label, id, value) in [
            (
                "RuneLite",
                "wrap_rl",
                &mut self.config.runelite_launch_command,
            ),
            ("RS3", "wrap_rs3", &mut self.config.rs3_launch_command),
            (
                "Old School",
                "wrap_osrs",
                &mut self.config.osrs_launch_command,
            ),
        ] {
            ui.horizontal(|ui| {
                ui.label(format!("{label}:"));
                changed |= optional_text(ui, id, value);
            });
        }

        ui.add_space(14.0);
        if ui.button("Back").clicked() {
            self.show_settings = false;
        }

        if changed {
            self.save_config();
        }
    }

    fn log_pane(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label("Log");
            if self.is_busy() {
                ui.spinner();
            }
        });
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for line in self.log.snapshot() {
                    ui.monospace(line);
                }
            });
    }
}

/// A text field bound to an `Option<String>`, where empty means `None`.
fn optional_text(ui: &mut egui::Ui, id: &str, value: &mut Option<String>) -> bool {
    let mut text = value.clone().unwrap_or_default();
    let response = ui.add(
        egui::TextEdit::singleline(&mut text)
            .id_salt(id)
            .desired_width(f32::INFINITY),
    );
    if response.changed() {
        *value = Some(text).filter(|t| !t.trim().is_empty());
        true
    } else {
        false
    }
}

/// Runs the login window as a child process and reads back the session it prints.
///
/// The webview needs a GTK main loop of its own, which cannot coexist with the one
/// driving this window — see the module docs in `main.rs`.
fn run_login_window() -> anyhow::Result<Session> {
    use anyhow::{Context, bail};

    let exe = std::env::current_exe().context("could not locate the rsclient binary")?;
    let output = std::process::Command::new(exe)
        .arg("login")
        .output()
        .context("could not start the sign-in window")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let message = stderr.trim();
        bail!(
            "sign-in failed: {}",
            if message.is_empty() {
                "the sign-in window exited unexpectedly"
            } else {
                message
            }
        );
    }

    serde_json::from_slice(&output.stdout)
        .context("could not read the session from the sign-in window")
}
