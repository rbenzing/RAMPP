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
            ui.heading("RAMP");
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
                false,
                false,
                mysql_running,
                php_running,
                apache_running,
            );
            service_row(
                ui,
                &self.tx,
                Service::Mysql,
                &state.mysql,
                state.config.mysql.port,
                state.phpmyadmin_enabled,
                state.phpmyadmin_dir_exists,
                mysql_running,
                php_running,
                apache_running,
            );
            service_row(
                ui,
                &self.tx,
                Service::Php,
                &state.php,
                state.config.php.port,
                false,
                false,
                mysql_running,
                php_running,
                apache_running,
            );

            ui.separator();

            ui.horizontal(|ui| {
                if ui.button("Start All").clicked() {
                    let _ = self.tx.send(Event::StartService(Service::Apache));
                    let _ = self.tx.send(Event::StartService(Service::Mysql));
                    let _ = self.tx.send(Event::StartService(Service::Php));
                }
                if ui.button("Stop All").clicked() {
                    let _ = self.tx.send(Event::StopService(Service::Apache));
                    let _ = self.tx.send(Event::StopService(Service::Mysql));
                    let _ = self.tx.send(Event::StopService(Service::Php));
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
            ui.label("Log");

            let lines = self.log.tail(100);
            egui::ScrollArea::vertical()
                .stick_to_bottom(true)
                .max_height(300.0)
                .show(ui, |ui| {
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
    phpmyadmin_enabled: bool,
    phpmyadmin_dir_exists: bool,
    mysql_running: bool,
    php_running: bool,
    apache_running: bool,
) {
    ui.horizontal(|ui| {
        let dot_color = state_indicator(status.state);
        ui.colored_label(dot_color, "●");
        ui.label(format!("{svc}"));
        ui.label(format!("[{}]", status.state));

        // Show effective port — yellow when remapped from the configured value.
        if let Some(eff) = status.effective_port {
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

        // Show elapsed startup time
        if status.state == ServiceState::Starting {
            if let Some(start) = status.started_at {
                ui.label(format!("({}s)", start.elapsed().as_secs()));
            }
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
            }
            ui.colored_label(egui::Color32::GRAY, "(Start to retry)");
        } else if let Some(err) = &status.last_error {
            let short = truncate_error(err);
            ui.colored_label(egui::Color32::RED, format!("⚠ {short}"))
                .on_hover_text(err.as_str());
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("Stop").clicked() {
                let _ = tx.send(Event::StopService(svc));
            }
            if ui.button("Restart").clicked() {
                let _ = tx.send(Event::RestartService(svc));
            }
            if ui.button("Start").clicked() {
                let _ = tx.send(Event::StartService(svc));
            }

            // Admin button — only for MySQL row
            if svc == Service::Mysql {
                let all_up = mysql_running && php_running && apache_running;
                let can_admin = all_up && phpmyadmin_dir_exists;

                let btn_label = if phpmyadmin_enabled {
                    "Admin ■"
                } else {
                    "Admin ▶"
                };

                let btn = egui::Button::new(btn_label);
                let response = ui.add_enabled(can_admin, btn);

                let clicked = response.clicked();

                if !can_admin {
                    let tooltip = if !phpmyadmin_dir_exists {
                        "phpMyAdmin not found in install directory"
                    } else {
                        "MySQL, PHP, and Apache must all be running"
                    };
                    response.on_disabled_hover_text(tooltip);
                }

                if clicked {
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
