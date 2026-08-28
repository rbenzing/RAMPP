use crate::config::load_config;
use crate::events::Event;
use crate::logger::SharedLog;
use crate::state::{AppState, Service, ServiceState, HEALTH_FAIL_THRESHOLD};
use crossbeam_channel::Sender;
use eframe::egui;
use std::sync::{Arc, Mutex};

pub struct RampApp {
    state: Arc<Mutex<AppState>>,
    tx: Sender<Event>,
    log: SharedLog,
    show_window_rx: crossbeam_channel::Receiver<()>,
}

impl RampApp {
    pub fn new(
        state: Arc<Mutex<AppState>>,
        tx: Sender<Event>,
        log: SharedLog,
        show_window_rx: crossbeam_channel::Receiver<()>,
    ) -> Self {
        Self {
            state,
            tx,
            log,
            show_window_rx,
        }
    }
}

impl eframe::App for RampApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Show window if tray requested it
        if self.show_window_rx.try_recv().is_ok() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }

        ctx.request_repaint_after(std::time::Duration::from_secs(1));

        let state = match self.state.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => {
                // Event loop panicked while holding the lock — recover last known state
                // so the UI keeps rendering rather than crashing.
                log::error!("state mutex poisoned — event loop may have crashed");
                poisoned.into_inner().clone()
            }
        };

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("RAMPP");
            ui.separator();

            let mysql_running = state.mysql.state == ServiceState::Running;
            let php_running = state.php.state == ServiceState::Running;
            let apache_running = state.apache.state == ServiceState::Running;

            service_row(
                ui,
                &self.tx,
                Service::Apache,
                &state.apache,
                state.config.apache.port,
                state.ports.assigned(Service::Apache),
                false,
                false,
                false,
                mysql_running,
                php_running,
                apache_running,
                &state.config.apache.conf,
            );
            service_row(
                ui,
                &self.tx,
                Service::Mysql,
                &state.mysql,
                state.config.mysql.port,
                state.ports.assigned(Service::Mysql),
                state.phpmyadmin_enabled,
                state.phpmyadmin_dir_exists,
                true,
                mysql_running,
                php_running,
                apache_running,
                &state.config.mysql.ini,
            );
            service_row(
                ui,
                &self.tx,
                Service::Php,
                &state.php,
                state.config.php.port,
                state.ports.assigned(Service::Php),
                false,
                false,
                false,
                mysql_running,
                php_running,
                apache_running,
                &state.config.php.ini,
            );

            ui.separator();

            ui.horizontal(|ui| {
                // Single toggle button: Start All when any service is stoppable, Stop All otherwise
                let any_active = [&state.apache, &state.mysql, &state.php].iter().any(|s| {
                    matches!(
                        s.state,
                        ServiceState::Running | ServiceState::Starting | ServiceState::Stopping
                    )
                });
                let all_stop_label = if any_active {
                    "■ Stop All"
                } else {
                    "▶ Start All"
                };
                if ui.button(all_stop_label).clicked() {
                    if any_active {
                        let _ = self.tx.send(Event::StopService(Service::Apache));
                        let _ = self.tx.send(Event::StopService(Service::Mysql));
                        let _ = self.tx.send(Event::StopService(Service::Php));
                    } else {
                        let _ = self.tx.send(Event::StartService(Service::Apache));
                        let _ = self.tx.send(Event::StartService(Service::Mysql));
                        let _ = self.tx.send(Event::StartService(Service::Php));
                    }
                }
                if ui.button("Reload Config").clicked() {
                    match load_config(&state.config.install_dir) {
                        Ok(new_config) => {
                            let _ = self.tx.send(Event::ConfigReloaded(Box::new(new_config)));
                        }
                        Err(e) => {
                            log::error!("config reload failed: {e}");
                            self.log.push(format!("ERROR: config reload failed — {e}"));
                        }
                    }
                }
            });

            ui.separator();
            ui.horizontal(|ui| {
                ui.label("Document Root:");
                ui.monospace(state.config.apache.document_root.display().to_string());
                if ui.button("📁 Change…").clicked() {
                    if let Some(folder) = rfd::FileDialog::new()
                        .set_title("Select Apache Document Root")
                        .pick_folder()
                    {
                        match crate::paths::validate_document_root(&folder) {
                            Ok(()) => {
                                let _ = self.tx.send(Event::SetDocumentRoot(folder));
                            }
                            Err(e) => {
                                log::error!("invalid document root: {e}");
                                self.log.push(format!("ERROR: invalid document root — {e}"));
                            }
                        }
                    }
                }
            });

            ui.separator();
            ui.horizontal(|ui| {
                ui.label("Log");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("✕ Clear").clicked() {
                        self.log.clear();
                        let _ = self.tx.send(Event::ClearLog);
                    }
                    if ui.small_button("⎘ Copy").clicked() {
                        let text = self.log.tail(100).join("\n");
                        ctx.copy_text(text);
                    }
                });
            });

            let lines = self.log.tail(100);
            egui::ScrollArea::vertical()
                .stick_to_bottom(true)
                .max_height(300.0)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    for line in &lines {
                        ui.monospace(line);
                    }
                });
        });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        let _ = self.tx.send(Event::ShutdownAll);
    }
}

#[allow(clippy::too_many_arguments)]
fn service_row(
    ui: &mut egui::Ui,
    tx: &Sender<Event>,
    svc: Service,
    status: &crate::state::ServiceStatus,
    configured_port: u16,
    assigned_port: Option<u16>,
    phpmyadmin_enabled: bool,
    phpmyadmin_dir_exists: bool,
    show_admin: bool,
    mysql_running: bool,
    php_running: bool,
    apache_running: bool,
    config_path: &std::path::Path,
) {
    ui.horizontal(|ui| {
        let dot_color = state_indicator(status.state);
        ui.colored_label(dot_color, "●");
        ui.label(format!("{svc}"));
        ui.label(format!("[{}]", status.state));

        // Show effective port — yellow when remapped from the configured value.
        if let Some(eff) = assigned_port {
            if eff == configured_port {
                ui.label(format!(":{eff}"));
            } else {
                ui.colored_label(
                    egui::Color32::YELLOW,
                    format!(":{eff} (cfg {configured_port})"),
                )
                .on_hover_text("Configured port was in use; bound to next free port");
            }
        }

        match status.state {
            ServiceState::Starting => {
                if let Some(start) = status.started_at {
                    ui.label(format!("({}s)", start.elapsed().as_secs()));
                }
            }
            ServiceState::Running => {
                if let Some(start) = status.started_at {
                    let secs = start.elapsed().as_secs();
                    ui.colored_label(
                        egui::Color32::DARK_GRAY,
                        format!("up {}", format_uptime(secs)),
                    );
                }
            }
            _ => {}
        }

        // Show health degradation before the service crashes
        if status.state == ServiceState::Running && status.health_fail_streak > 0 {
            ui.colored_label(
                egui::Color32::YELLOW,
                format!(
                    "⚠ health {}/{}",
                    status.health_fail_streak, HEALTH_FAIL_THRESHOLD
                ),
            );
        }

        // Show last error and recovery hint — truncated inline, full text on hover
        if status.state == ServiceState::Error {
            if let Some(err) = &status.last_error {
                let short = truncate_error(err);
                ui.colored_label(egui::Color32::RED, format!("⚠ {short}"))
                    .on_hover_text(err.as_str());
                if ui
                    .small_button("✕")
                    .on_hover_text("Dismiss error")
                    .clicked()
                {
                    let _ = tx.send(Event::DismissError(svc));
                }
            }
            ui.colored_label(egui::Color32::GRAY, "(click ▶ to retry)");
        } else if let Some(err) = &status.last_error {
            let short = truncate_error(err);
            ui.colored_label(egui::Color32::RED, format!("⚠ {short}"))
                .on_hover_text(err.as_str());
            if ui
                .small_button("✕")
                .on_hover_text("Dismiss error")
                .clicked()
            {
                let _ = tx.send(Event::DismissError(svc));
            }
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // Start/Stop toggle — single button, label reflects current state
            let is_running = matches!(
                status.state,
                ServiceState::Running | ServiceState::Starting | ServiceState::Stopping
            );
            let toggle_label = if is_running { "■ Stop" } else { "▶ Start" };
            if ui.button(toggle_label).clicked() {
                if is_running {
                    let _ = tx.send(Event::StopService(svc));
                } else {
                    let _ = tx.send(Event::StartService(svc));
                }
            }

            // Restart — only useful when running
            let restart_btn = ui.add_enabled(
                status.state == ServiceState::Running,
                egui::Button::new("↺ Restart"),
            );
            if restart_btn.clicked() {
                let _ = tx.send(Event::RestartService(svc));
            }

            if ui.button("✎ Edit Config").clicked() {
                open_in_editor(config_path);
            }

            // Open in browser — Apache only
            if svc == Service::Apache {
                let port = assigned_port.unwrap_or(configured_port);
                let is_running = status.state == ServiceState::Running;
                let open_resp = ui.add_enabled(is_running, egui::Button::new("↗ Open"));
                let clicked = open_resp.clicked();
                if !is_running {
                    open_resp.on_disabled_hover_text("Apache must be Running");
                }
                if clicked {
                    open_in_browser(&format!("http://localhost:{port}"));
                }
            }

            // Admin controls — only on the MySQL row when show_admin is true
            if show_admin {
                let all_up = mysql_running && php_running && apache_running;
                let can_admin = all_up && phpmyadmin_dir_exists;
                let disabled_tooltip = if !phpmyadmin_dir_exists {
                    "phpMyAdmin not found in install directory"
                } else {
                    "MySQL, PHP, and Apache must all be running"
                };

                // Open button — only active when admin is enabled and services are up
                let open_active = can_admin && phpmyadmin_enabled;
                let open_btn = ui.add_enabled(open_active, egui::Button::new("↗ Open"));
                let open_clicked = open_btn.clicked();
                if !open_active {
                    open_btn.on_disabled_hover_text(if phpmyadmin_enabled {
                        disabled_tooltip
                    } else {
                        "Enable Admin first"
                    });
                }
                if open_clicked {
                    let _ = tx.send(Event::OpenPhpMyAdmin);
                }

                // Admin toggle button
                let admin_label = if phpmyadmin_enabled {
                    "Admin ■"
                } else {
                    "Admin ▶"
                };
                let admin_btn = ui.add_enabled(can_admin, egui::Button::new(admin_label));
                let admin_clicked = admin_btn.clicked();
                if !can_admin {
                    admin_btn.on_disabled_hover_text(disabled_tooltip);
                }
                if admin_clicked {
                    let _ = tx.send(Event::TogglePhpMyAdmin);
                }
            }
        });
    });
}

fn truncate_error(msg: &str) -> &str {
    const MAX: usize = 40;
    if msg.len() <= MAX {
        msg
    } else {
        // Truncate at a char boundary
        let mut idx = MAX;
        while !msg.is_char_boundary(idx) {
            idx -= 1;
        }
        &msg[..idx]
    }
}

fn state_indicator(state: ServiceState) -> egui::Color32 {
    match state {
        ServiceState::Running => egui::Color32::GREEN,
        ServiceState::Starting | ServiceState::Stopping => egui::Color32::YELLOW,
        ServiceState::Crashed | ServiceState::Error => egui::Color32::RED,
        ServiceState::Stopped => egui::Color32::GRAY,
    }
}

fn format_uptime(elapsed_secs: u64) -> String {
    if elapsed_secs < 60 {
        format!("{elapsed_secs}s")
    } else if elapsed_secs < 3600 {
        format!("{}m {}s", elapsed_secs / 60, elapsed_secs % 60)
    } else {
        format!("{}h {}m", elapsed_secs / 3600, (elapsed_secs % 3600) / 60)
    }
}

fn open_in_editor(path: &std::path::Path) {
    let _ = std::process::Command::new("cmd")
        .args(["/c", "start", "", &path.to_string_lossy()])
        .spawn();
}

fn open_in_browser(url: &str) {
    let _ = std::process::Command::new("cmd")
        .args(["/c", "start", "", url])
        .spawn();
}
