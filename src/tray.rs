use crate::events::Event;
use crate::state::Service;
use crossbeam_channel::Sender;
use tray_item::{IconSource, TrayItem};

pub fn run_tray(tx: Sender<Event>, show_window_tx: crossbeam_channel::Sender<()>) {
    let mut tray = match TrayItem::new("RAMPP", IconSource::Resource("icon")) {
        Ok(t) => t,
        Err(e) => {
            log::error!("failed to create system tray: {e}");
            return;
        }
    };

    let show_tx = show_window_tx.clone();
    tray.add_menu_item("Open RAMPP", move || {
        let _ = show_tx.send(());
    })
    .unwrap_or_else(|e| log::warn!("tray: could not add 'Open RAMPP' item: {e}"));

    tray.add_label("─────────────")
        .unwrap_or_else(|e| log::warn!("tray: could not add separator: {e}"));

    let tx2 = tx.clone();
    tray.add_menu_item("Start All", move || {
        let _ = tx2.send(Event::StartService(Service::Apache));
        let _ = tx2.send(Event::StartService(Service::Mysql));
        let _ = tx2.send(Event::StartService(Service::Php));
    })
    .unwrap_or_else(|e| log::warn!("tray: could not add 'Start All' item: {e}"));

    let tx3 = tx.clone();
    tray.add_menu_item("Stop All", move || {
        let _ = tx3.send(Event::StopService(Service::Apache));
        let _ = tx3.send(Event::StopService(Service::Mysql));
        let _ = tx3.send(Event::StopService(Service::Php));
    })
    .unwrap_or_else(|e| log::warn!("tray: could not add 'Stop All' item: {e}"));

    tray.add_label("─────────────")
        .unwrap_or_else(|e| log::warn!("tray: could not add separator: {e}"));

    let tx4 = tx.clone();
    tray.add_menu_item("Exit", move || {
        let _ = tx4.send(Event::ShutdownAll);
    })
    .unwrap_or_else(|e| log::warn!("tray: could not add 'Exit' item: {e}"));

    // tray-item's inner loop — blocks until the tray is destroyed
    loop {
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}
