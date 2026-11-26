#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]
// SPDX-License-Identifier: GPL-3.0-or-later

mod licenses;
mod terminal;

use nbt_installer_core::{
    DEFAULT_HOST, DEFAULT_PASSWORD, DEFAULT_PORT, DEFAULT_USER, activation, bench, bundle, ssh,
};

use eframe::egui::{self, FontId, RichText};
use std::{io::Write as _, sync::mpsc, time::Duration};
use tokio::sync::{mpsc::UnboundedSender, oneshot};
use zeroize::Zeroizing;

/// Log lines kept on screen; older ones are dropped (the log file keeps all).
const LOG_LINES: usize = 2000;

struct Line {
    /// Seconds since the installer started.
    at: f32,
    /// Diagnostic detail, shown only with the debug log on.
    detail: bool,
    text: String,
}

struct App {
    licenses: licenses::View,
    host: String,
    port: String,
    user: String,
    password: Zeroizing<String>,
    /// The vehicle gateway for enabling SSH; empty searches the cable.
    gateway: String,
    log: Vec<Line>,
    /// Show diagnostic detail and write the log to a file; off by default.
    debug: bool,
    receiver: Option<mpsc::Receiver<ssh::Event>>,
    commands: Option<UnboundedSender<ssh::Command>>,
    activation_confirmation: Option<(String, Option<String>, oneshot::Sender<bool>)>,
    connected: bool,
    target: bool,
    bundle: Option<bundle::Bundle>,
    updating: bool,
    /// The current operation's step: index from 0, step count and label.
    phase: Option<(usize, usize, String)>,
    /// Bytes sent and to send in the current step.
    progress: (usize, usize),
    /// When the current transfer first reported progress, and how far it was.
    progress_since: Option<(std::time::Instant, usize)>,
    test_app: Option<ssh::TestApp>,
    controller_config: String,
    confirm_removal: bool,
    /// The head unit has to restart to load the new plugin: offer it once the
    /// install or update has finished.
    restart_prompt: bool,
    /// With the debug log on, every line is also written here.
    log_file: Option<std::fs::File>,
    started: std::time::Instant,
}

/* Where the installer log and fetched debug logs go: Documents\NBT Manager logs, else the app's data folder. */
fn log_folder() -> Option<std::path::PathBuf> {
    directories::UserDirs::new()
        .and_then(|dirs| dirs.document_dir().map(|dir| dir.join("NBT Manager logs")))
        .or_else(|| {
            directories::ProjectDirs::from("net", "nbt-manager", "installer")
                .map(|dirs| dirs.data_local_dir().join("logs"))
        })
}

impl Default for App {
    fn default() -> Self {
        let (bundle, release_message) = bundle::load();
        let test_app = bundle::test_app();
        Self {
            licenses: licenses::View::default(),
            host: DEFAULT_HOST.into(),
            port: DEFAULT_PORT.to_string(),
            user: DEFAULT_USER.into(),
            password: Zeroizing::new(DEFAULT_PASSWORD.to_owned()),
            gateway: String::new(),
            log: [
                "Address and password are prefilled: press Connect. If SSH does not answer, the installer offers to enable it.".to_owned(),
                release_message,
            ]
            .into_iter()
            .map(|text| Line { at: 0.0, detail: false, text })
            .collect(),
            debug: false,
            receiver: None,
            commands: None,
            activation_confirmation: None,
            connected: false,
            target: false,
            bundle,
            updating: false,
            phase: None,
            progress: (0, 0),
            progress_since: None,
            test_app,
            controller_config: bench::CONFIG_EXAMPLE.to_owned(),
            confirm_removal: false,
            restart_prompt: false,
            log_file: None,
            started: std::time::Instant::now(),
        }
    }
}

impl App {
    fn log(&mut self, message: impl Into<String>) {
        self.push(false, message.into());
    }

    /// A diagnostic line, shown and saved only with the debug log on.
    fn detail(&mut self, message: impl Into<String>) {
        self.push(true, message.into());
    }

    fn push(&mut self, detail: bool, text: String) {
        let line = Line {
            at: self.started.elapsed().as_secs_f32(),
            detail,
            text,
        };
        if let Some(file) = &mut self.log_file {
            let _ = writeln!(file, "[{:8.1}s] {}", line.at, line.text);
        }
        if self.log.len() == LOG_LINES {
            self.log.remove(0);
        }
        self.log.push(line);
    }

    /* The debug log checkbox. Detail is always recorded, so switching it on
     * after a failure still shows what led to it; the file then starts with
     * every line kept so far. */
    fn set_debug(&mut self, on: bool) {
        self.debug = on;
        if !on {
            self.log_file = None;
            return;
        }
        let Some(folder) = log_folder() else {
            self.log("Cannot find a folder for the installer log file.");
            return;
        };
        let seconds = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let path = folder.join(format!("installer-{seconds}.log"));
        match std::fs::create_dir_all(&folder).and_then(|_| std::fs::File::create(&path)) {
            Ok(mut file) => {
                for line in &self.log {
                    let _ = writeln!(file, "[{:8.1}s] {}", line.at, line.text);
                }
                self.log_file = Some(file);
                self.log(format!("Debug log on; also written to {}", path.display()));
            }
            Err(error) => self.log(format!(
                "Could not create the log file in {}: {error}",
                folder.display()
            )),
        }
    }

    fn save_log(&mut self, system: bool, data: &[u8]) {
        if system {
            self.save_file("system log", "slog", "log", data);
        } else {
            self.save_file("debug log", "debug", "log", data);
        }
    }

    fn save_file(&mut self, what: &str, prefix: &str, extension: &str, data: &[u8]) {
        let Some(folder) = log_folder() else {
            self.log(format!("Cannot find a folder to save the {what} in."));
            return;
        };
        let seconds = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let path = folder.join(format!("{prefix}-{seconds}.{extension}"));
        match std::fs::create_dir_all(&folder).and_then(|_| std::fs::write(&path, data)) {
            Ok(()) => self.log(format!("The {what} was saved to {}", path.display())),
            Err(_) => self.log(format!("Could not save the {what} in {}", folder.display())),
        }
    }

    fn open_terminal(&mut self) {
        let Some(args) = terminal::ssh_args(self.host.trim(), self.port.trim(), self.user.trim())
        else {
            self.log("Check the address, port and username in Connection settings.");
            return;
        };
        match terminal::open(&args, &self.password) {
            Ok(()) => self.log(format!("Opened a terminal: ssh {}", args.join(" "))),
            Err(error) => self.log(format!("Could not open a terminal with ssh: {error}")),
        }
    }

    fn bench(&mut self, action: bench::Action) {
        if let Some(commands) = &self.commands
            && commands
                .send(ssh::Command::Bench {
                    action,
                    test_app: self.test_app.clone(),
                })
                .is_ok()
        {
            self.start_transfer();
        }
    }

    fn start_transfer(&mut self) {
        self.updating = true;
        self.phase = None;
        self.progress = (0, 0);
        self.progress_since = None;
    }

    /// The overall bar: finished steps plus the current step's share.
    fn overall(&self) -> Option<(f32, String)> {
        let (index, count, label) = self.phase.as_ref()?;
        let fill = (*index as f32 + self.progress_text().0) / (*count).max(1) as f32;
        Some((
            fill.min(1.0),
            format!("Step {} of {count}: {label}", index + 1),
        ))
    }

    /// The progress bar's fill and text: bytes sent, speed and time left.
    fn progress_text(&self) -> (f32, String) {
        let (sent, total) = self.progress;
        if total == 0 {
            return (0.0, "Working…".into());
        }
        if sent >= total {
            return (
                1.0,
                format!("{} sent; waiting for the head unit…", size(total)),
            );
        }
        let mut text = format!("{} of {} sent", size(sent), size(total));
        if let Some((since, from)) = self.progress_since {
            let seconds = since.elapsed().as_secs_f64();
            let rate = (sent - from) as f64 / seconds;
            if seconds >= 1.0 && rate > 0.0 {
                let left = ((total - sent) as f64 / rate).ceil() as u64;
                text += &format!(
                    " · {}/s · about {}:{:02} left",
                    size(rate as usize),
                    left / 60,
                    left % 60
                );
            }
        }
        (sent as f32 / total as f32, text)
    }

    /// The log as shown and copied: detail only with the debug log on.
    fn log_text(&self) -> String {
        let mut text = String::new();
        for line in self.log.iter().filter(|line| self.debug || !line.detail) {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&line.text);
        }
        text
    }

    fn connect(&mut self) {
        let host = self.host.trim().to_ascii_lowercase();
        let port = self.port.parse::<u16>().ok().filter(|value| *value > 0);
        if !ssh::valid_host(&host)
            || port.is_none()
            || self.user.is_empty()
            || self.user.len() > 128
            || self.user.chars().any(char::is_control)
            || self.password.is_empty()
            || self.password.len() > 1024
        {
            self.log("Enter a valid host, port, username and password.");
            return;
        }
        let Some(gateway) = parse_gateway(&self.gateway) else {
            self.log("Enter the gateway address as four numbers, such as 169.254.24.4, or leave it empty to search for it.");
            return;
        };
        let settings = ssh::Settings {
            host,
            port: port.unwrap(),
            user: self.user.clone(),
            // Copy, not take: the field keeps the password for the next attempt.
            password: Zeroizing::new(self.password.as_str().to_owned()),
            gateway,
        };
        let (send, receive) = mpsc::channel();
        self.commands = Some(ssh::start(settings, send));
        self.receiver = Some(receive);
    }

    fn poll(&mut self) {
        let mut events = Vec::new();
        if let Some(receiver) = &self.receiver {
            loop {
                match receiver.try_recv() {
                    Ok(event) => events.push(event),
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        events.push(ssh::Event::Finished("SSH worker stopped."));
                        break;
                    }
                }
            }
        }
        for event in events {
            match event {
                ssh::Event::Message(message) => self.log(message),
                ssh::Event::ConfirmActivation {
                    host,
                    hint,
                    respond,
                } => self.activation_confirmation = Some((host, hint, respond)),
                ssh::Event::Note(message) => self.log(message),
                ssh::Event::Detail(message) => self.detail(message),
                ssh::Event::Connected => {
                    self.connected = true;
                    self.log("SSH authentication succeeded.");
                }
                ssh::Event::Phase {
                    index,
                    count,
                    label,
                    bytes,
                } => {
                    self.phase = Some((index, count, label));
                    self.progress = (0, bytes);
                    self.progress_since = None;
                }
                ssh::Event::Target(target) => {
                    self.target = target;
                    self.phase = None;
                }
                ssh::Event::Progress { sent, total } => {
                    if self.progress_since.is_none() {
                        self.progress_since = Some((std::time::Instant::now(), sent));
                    }
                    self.progress = (sent, total);
                }
                ssh::Event::InstallFinished(success) => {
                    self.phase = None;
                    self.updating = false;
                    // The connection stays usable: every operation starts afresh.
                    self.log(if success {
                        "Manager and iDrive plugin installed. iDrive loads the plugin after a restart."
                    } else {
                        "Installation did not complete; the reason is above. Install can be retried at once."
                    });
                }
                ssh::Event::UpdateFinished(success) => {
                    self.phase = None;
                    self.updating = false;
                    self.log(if success {
                        "Update staged. Open NBT Manager and keep it open for at least 10 seconds to confirm the update."
                    } else {
                        "The update could not be staged; the reason is above."
                    });
                }
                ssh::Event::RestartNeeded => self.restart_prompt = true,
                ssh::Event::BenchFinished { ok, message } => {
                    self.phase = None;
                    self.updating = false;
                    self.log(if ok {
                        message
                    } else {
                        format!("Failed: {message}")
                    });
                }
                ssh::Event::LogFetched { system, data } => self.save_log(system, &data),
                ssh::Event::Collected(data) => {
                    self.save_file("collection", "collect", "txt", &data)
                }
                ssh::Event::Finished(message) => {
                    self.log(message);
                    self.phase = None;
                    self.connected = false;
                    self.target = false;
                    self.updating = false;
                    self.receiver = None;
                    self.commands = None;
                    self.activation_confirmation = None;
                    self.restart_prompt = false;
                }
            }
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll();
        self.licenses.show(ui.ctx());
        if self.receiver.is_some() {
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        }
        egui::CentralPanel::default().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading("NBT Manager Installer");
                if ui.button("Licenses").clicked() { self.licenses.open(); }
            });
            ui.label(RichText::new(licenses::HEAD_UNIT_WARNING).color(ui.visuals().warn_fg_color));
            ui.add_space(8.0);
            // The connection details are built in; shown only on request.
            egui::CollapsingHeader::new("Connection settings")
                .default_open(false)
                .show(ui, |ui| ui.add_enabled_ui(self.receiver.is_none(), |ui| {
                egui::Grid::new("connection").num_columns(2).show(ui, |ui| {
                    ui.label("Address");
                    ui.text_edit_singleline(&mut self.host);
                    ui.end_row();
                    ui.label("Port");
                    ui.text_edit_singleline(&mut self.port);
                    ui.end_row();
                    ui.label("Username");
                    ui.text_edit_singleline(&mut self.user);
                    ui.end_row();
                    // Shown in clear text on purpose: this is the NBT-EVO's
                    // published default, not a personal secret, and masking it
                    // stopped it being read back, copied or corrected.
                    ui.label("Password");
                    ui.text_edit_singleline(&mut *self.password);
                    ui.end_row();
                    ui.label("Gateway");
                    ui.add(egui::TextEdit::singleline(&mut self.gateway).hint_text("searched on the cable"))
                        .on_hover_text("Only for enabling SSH. Leave empty to search for the car's gateway. Type its address when the search finds nothing, for example after a Wi-Fi ENET adapter gave the car an address of its own.");
                    ui.end_row();
                });
            }));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(self.receiver.is_none(), egui::Button::new("Connect"))
                    .clicked()
                {
                    self.connect();
                }
                if ui
                    .button("Open SSH terminal")
                    .on_hover_text("Opens ssh to the head unit in a terminal window, with the connection settings above.")
                    .clicked()
                {
                    self.open_terminal();
                }
                if ui
                    .add_enabled(
                        self.receiver.is_some()
                            && self.activation_confirmation.is_none()
                            && !self.updating,
                        egui::Button::new("Disconnect"),
                    )
                    .clicked()
                    && let Some(commands) = &self.commands
                {
                    let _ = commands.send(ssh::Command::Disconnect);
                }
                if ui
                    .add_enabled(
                        self.connected && self.target && self.bundle.is_some() && !self.updating,
                        egui::Button::new("Install"),
                    )
                    .clicked()
                    && let (Some(commands), Some(bundle)) = (&self.commands, &self.bundle)
                    && commands
                        .send(bundle.install())
                        .is_ok()
                {
                    self.start_transfer();
                }
                if ui
                    .add_enabled(
                        self.connected && self.target && self.bundle.is_some() && !self.updating,
                        egui::Button::new("Update"),
                    )
                    .clicked()
                    && let (Some(commands), Some(bundle)) = (&self.commands, &self.bundle)
                    && commands
                        .send(bundle.update())
                        .is_ok()
                {
                    self.start_transfer();
                }
                if self.receiver.is_some() && !self.connected {
                    ui.spinner();
                }
            });
            let tools = self.connected && self.target && !self.updating;
            egui::CollapsingHeader::new("Test tools").show(ui, |ui| {
                ui.add_enabled_ui(tools, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        if ui
                            .button("Collect everything")
                            .on_hover_text("Saves one file with the debug and system logs, the applications' logs, the audio connections, the processes and the manager's files. Reads only.")
                            .clicked()
                        {
                            self.bench(bench::Action::CollectAll);
                        }
                    });
                    ui.horizontal_wrapped(|ui| {
                        ui.label("Head unit debug log:");
                        if ui.button("Switch on").clicked() { self.bench(bench::Action::LogOn); }
                        if ui.button("Save to this computer").clicked() { self.bench(bench::Action::FetchLog); }
                        if ui.button("Switch off and delete").clicked() { self.bench(bench::Action::LogOff); }
                    });
                    ui.horizontal_wrapped(|ui| {
                        ui.label("Head unit system log:");
                        if ui
                            .button("Save to this computer")
                            .on_hover_text("Runs sloginfo on the iDrive processor and saves its output (iDrive's and other plugins' messages, such as Android Auto's).")
                            .clicked()
                        {
                            self.bench(bench::Action::FetchSystemLog);
                        }
                    });
                    ui.horizontal_wrapped(|ui| {
                        if ui
                            .button("Restart the head unit…")
                            .on_hover_text("Restarts the whole head unit (OnOffDSICommander appreset), as switching the car off and on. Asks first.")
                            .clicked()
                        {
                            self.restart_prompt = true;
                        }
                    });
                    ui.horizontal_wrapped(|ui| {
                        if ui
                            .button("Shim on")
                            .on_hover_text("Turns the automatic shim on again after it failed or was turned off.")
                            .clicked()
                        {
                            self.bench(bench::Action::Shim(true));
                        }
                        if ui
                            .button("Shim off")
                            .on_hover_text("iDrive starts without the shim from its next start, until Shim on.")
                            .clicked()
                        {
                            self.bench(bench::Action::Shim(false));
                        }
                    });
                    ui.horizontal_wrapped(|ui| {
                        if ui
                            .add_enabled(self.test_app.is_some(), egui::Button::new("Install test application v1"))
                            .on_disabled_hover_text("This installer was built without a test application.")
                            .clicked()
                        {
                            self.bench(bench::Action::ProvisionTestApp);
                        }
                        if ui.button("Remove NBT Manager…").clicked() { self.confirm_removal = true; }
                    });
                    // Folded by default: it is rarely needed and takes most of the panel.
                    egui::CollapsingHeader::new("Controller settings (manager/controller.conf)")
                        .default_open(false)
                        .show(ui, |ui| {
                            ui.add(
                                egui::TextEdit::multiline(&mut self.controller_config)
                                    .font(FontId::monospace(12.0))
                                    .desired_rows(6)
                                    .desired_width(f32::INFINITY),
                            );
                            ui.horizontal(|ui| {
                                if ui.button("Write controller.conf").clicked() {
                                    let text = self.controller_config.replace("\r\n", "\n");
                                    self.bench(bench::Action::WriteConfig(text));
                                }
                                if ui.button("Remove controller.conf").clicked() { self.bench(bench::Action::RemoveConfig); }
                            });
                        });
                });
            });
            ui.separator();
            // Two bars over the log: the whole operation, then its current step.
            if let Some((fill, text)) = self.overall() {
                ui.add(egui::ProgressBar::new(fill).text(text));
                let (fill, text) = self.progress_text();
                ui.add(egui::ProgressBar::new(fill).text(text).animate(fill < 1.0));
                ui.add_space(4.0);
            }
            ui.horizontal(|ui| {
                ui.label("Log");
                if ui.button("Copy log").clicked() {
                    ui.ctx().copy_text(self.log_text());
                }
                let mut debug = self.debug;
                if ui
                    .checkbox(&mut debug, "Debug log")
                    .on_hover_text("Also show each command sent to the head unit and its answers, and save the log to Documents\\NBT Manager logs.")
                    .changed()
                {
                    self.set_debug(debug);
                }
            });
            egui::ScrollArea::vertical()
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    ui.add(
                        egui::Label::new(
                            RichText::new(self.log_text()).font(FontId::monospace(13.0)),
                        )
                        .selectable(true),
                    );
                });
        });
        if let Some((host, hint, _)) = &self.activation_confirmation {
            let mut accept = None;
            egui::Window::new("Enable SSH?")
                .collapsible(false).resizable(false)
                .show(ui.ctx(), |ui| {
                    ui.label(format!("SSH did not answer at {host}; it is probably not enabled on the head unit yet."));
                    // The address cannot be reached from this computer at all,
                    // so enabling SSH would not help: say so before the choice.
                    if let Some(hint) = hint {
                        ui.add_space(4.0);
                        ui.label(RichText::new(hint).strong());
                        ui.add_space(4.0);
                    }
                    ui.label("Enabling it finds the car's gateway on the cable, sends it diagnostic commands and relays its answers to the same online service as the Bavarian SSH Client:");
                    ui.monospace(activation::SERVER);
                    ui.horizontal(|ui| {
                        if ui.button("Enable SSH").clicked() { accept = Some(true); }
                        if ui.button("Cancel").clicked() { accept = Some(false); }
                    });
                });
            if let Some(accept) = accept
                && let Some((_, _, respond)) = self.activation_confirmation.take()
            {
                let _ = respond.send(accept);
            }
        }
        // Shown when the install or update that asked for it has finished.
        if self.restart_prompt && !self.updating {
            let mut restart = None;
            let modal = egui::Modal::new(egui::Id::unique("restart-idrive")).show(ui.ctx(), |ui| {
                ui.set_max_width(360.0);
                ui.heading("Restart the head unit?");
                ui.add_space(4.0);
                ui.label("The whole head unit restarts, as when the car is switched off and on: the screen goes dark, SSH switches off and the installer disconnects. iDrive loads NBT Manager's new entry only after a restart. Switching the car off and on does the same.");
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Later").clicked() { restart = Some(false); }
                    if ui.button("Restart the head unit").clicked() { restart = Some(true); }
                });
            });
            if modal.should_close() {
                restart.get_or_insert(false);
            }
            if let Some(restart) = restart {
                self.restart_prompt = false;
                if restart && self.connected && self.target {
                    self.bench(bench::Action::RestartIdrive);
                }
            }
        }
        if self.confirm_removal {
            let mut choice = None;
            egui::Window::new("Remove NBT Manager?")
                .collapsible(false)
                .resizable(false)
                .show(ui.ctx(), |ui| {
                    ui.label("This deletes /var/opt/nbt-manager (the manager, the applications installed through it, their data and keys) and the iDrive plugin folder. iDrive's own files are not touched.");
                    ui.label("Reboot the head unit afterwards to unload the plugin.");
                    ui.horizontal(|ui| {
                        if ui.button("Remove everything").clicked() { choice = Some(true); }
                        if ui.button("Cancel").clicked() { choice = Some(false); }
                    });
                });
            if let Some(remove) = choice {
                self.confirm_removal = false;
                if remove && self.connected && self.target && !self.updating {
                    self.bench(bench::Action::RemoveAll);
                }
            }
        }
    }
}

/// The Gateway field: `Some(None)` when empty (search), `Some(Some(ip))` for
/// a unicast IPv4 address, `None` for anything else.
fn parse_gateway(text: &str) -> Option<Option<std::net::Ipv4Addr>> {
    let text = text.trim();
    if text.is_empty() {
        return Some(None);
    }
    let ip = text.parse::<std::net::Ipv4Addr>().ok()?;
    let usable =
        !(ip.is_unspecified() || ip.is_broadcast() || ip.is_multicast() || ip.is_loopback());
    usable.then_some(Some(ip))
}

/// A byte count for people: KB below one megabyte, else MB.
fn size(bytes: usize) -> String {
    if bytes < 1024 * 1024 {
        format!("{} KB", bytes.div_ceil(1024))
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

fn main() -> eframe::Result {
    // Started by ssh as its askpass program (terminal.rs): print the password.
    if let Some(password) = std::env::var_os(terminal::ASKPASS_PASSWORD) {
        println!("{}", password.to_string_lossy());
        return Ok(());
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([700.0, 700.0])
            .with_min_inner_size([420.0, 360.0]),
        ..Default::default()
    };
    eframe::run_native(
        "NBT Manager Installer",
        options,
        Box::new(|_| Ok(Box::<App>::default())),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_field_is_empty_or_one_unicast_address() {
        use std::net::Ipv4Addr;
        assert_eq!(parse_gateway("  "), Some(None));
        assert_eq!(
            parse_gateway(" 192.168.16.100 "),
            Some(Some(Ipv4Addr::new(192, 168, 16, 100)))
        );
        for bad in [
            "0.0.0.0",
            "255.255.255.255",
            "224.0.0.1",
            "127.0.0.1",
            "169.254.24",
            "car",
            "169.254.24.4:6801",
        ] {
            assert_eq!(parse_gateway(bad), None, "{bad}");
        }
    }

    #[test]
    fn background_install_events_update_progress_and_request_reboot() {
        let mut app = App::default();
        let (send, receive) = mpsc::channel();
        app.receiver = Some(receive);
        app.updating = true;
        send.send(ssh::Event::Connected).unwrap();
        send.send(ssh::Event::Target(true)).unwrap();
        send.send(ssh::Event::Progress { sent: 3, total: 4 })
            .unwrap();
        app.poll();
        assert!(app.connected && app.target && app.updating);
        assert_eq!(app.progress, (3, 4));
        assert_eq!(app.progress_text().0, 0.75);
        send.send(ssh::Event::RestartNeeded).unwrap();
        send.send(ssh::Event::InstallFinished(true)).unwrap();
        app.poll();
        // The connection stays usable for a restart or another try.
        assert!(app.connected && app.target && !app.updating);
        assert!(app.restart_prompt);
        assert!(app.log_text().contains("loads the plugin after a restart"));
        // Disconnecting withdraws the offer.
        send.send(ssh::Event::Finished("SSH connection closed."))
            .unwrap();
        app.poll();
        assert!(!app.restart_prompt);
    }

    #[test]
    fn failed_update_keeps_the_connection_and_logs_are_copyable_and_bounded() {
        let mut app = App::default();
        let (send, receive) = mpsc::channel();
        app.receiver = Some(receive);
        app.connected = true;
        app.target = true;
        app.updating = true;
        send.send(ssh::Event::UpdateFinished(false)).unwrap();
        app.poll();
        assert!(app.connected && app.target && !app.updating);
        assert!(app.log_text().contains("could not be staged"));
        for index in 0..LOG_LINES + 10 {
            app.log(format!("event {index}"));
        }
        assert_eq!(app.log.len(), LOG_LINES);
        let copied = app.log_text();
        assert!(!copied.contains("event 9\n"));
        assert!(copied.starts_with("event 10\n"));
        assert!(copied.ends_with(&format!("event {}", LOG_LINES + 9)));
    }

    #[test]
    fn detail_shows_only_with_the_debug_log_and_progress_reads_in_bytes() {
        let mut app = App::default();
        let (send, receive) = mpsc::channel();
        app.receiver = Some(receive);
        send.send(ssh::Event::Note("gateway found".into())).unwrap();
        send.send(ssh::Event::Detail(
            "Installing failed: exit status 1".into(),
        ))
        .unwrap();
        app.poll();
        assert!(!app.debug);
        assert!(app.log_text().ends_with("gateway found"));
        // Switching on later still shows detail recorded before (no file in tests).
        app.debug = true;
        assert!(
            app.log_text()
                .ends_with("gateway found\nInstalling failed: exit status 1")
        );

        app.start_transfer();
        assert_eq!(app.progress_text(), (0.0, "Working…".into()));
        assert_eq!(app.overall(), None);
        app.progress = (512 * 1024, 3 * 1024 * 1024);
        assert_eq!(app.progress_text().1, "512 KB of 3.0 MB sent");
        // Step 2 of 4, half sent: the overall bar is at (1 + 0.5) / 4.
        app.phase = Some((1, 4, "Sending and installing the manager".into()));
        app.progress = (1024 * 1024, 2 * 1024 * 1024);
        assert_eq!(
            app.overall(),
            Some((
                0.375,
                "Step 2 of 4: Sending and installing the manager".into()
            ))
        );
        app.progress = (512 * 1024, 3 * 1024 * 1024);
        app.progress = (3 * 1024 * 1024, 3 * 1024 * 1024);
        assert_eq!(
            app.progress_text().1,
            "3.0 MB sent; waiting for the head unit…"
        );
    }
}
