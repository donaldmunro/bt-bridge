//! The Bluetooth device list pane: scan, connect, trust, pair, forget.
//!
//! Replaces the merged-mode conflict tree. The default action for a fitness device is
//! a plain GATT connect plus trust - never bonding, which most LE fitness devices
//! don't support (and which is why generic Bluetooth wizards fail to connect them).

use bt_bridge_core::bluez::device::DeviceKind;
use eframe::egui::{self, RichText};

use crate::{shared::{Command, DeviceRow, SharedState},
            ui::theme};

pub fn show(ui: &mut egui::Ui, state: &SharedState) -> Option<Command>
{
   let mut command = None;

   ui.horizontal(|ui| {
      ui.label(theme::section_header("Bluetooth devices"));
      ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
         if state.scanning
         {
            let stop = egui::Button::new(RichText::new("Stop scan").color(theme::TEXT_SECONDARY))
                          .stroke(egui::Stroke::new(1.0, theme::BORDER));
            if ui.add(stop).clicked()
            {
               command = Some(Command::StopScan);
            }
            ui.add(egui::Spinner::new().size(13.0));
            ui.label(RichText::new("scanning").color(theme::TEXT_DIM).size(11.5));
         }
         else
         {
            let scan = egui::Button::new(RichText::new("Scan").color(theme::TEXT_SECONDARY))
                          .stroke(egui::Stroke::new(1.0, theme::BORDER));
            if ui.add(scan).clicked()
            {
               command = Some(Command::StartScan);
            }
         }
      });
   });
   ui.separator();

   egui::ScrollArea::vertical().auto_shrink([false, false]).id_salt("bt-devices").show(ui, |ui| {
      if state.devices.is_empty()
      {
         ui.add_space(12.0);
         ui.label(RichText::new("No Bluetooth devices known.").color(theme::TEXT_DIM));
         ui.label(RichText::new("Wake your devices (spin the pedals, power the trainer, \
                                 wear the strap) and press Scan.")
                     .color(theme::TEXT_DISABLED)
                     .size(12.0));
         return;
      }

      for (i, device) in state.devices.iter().enumerate()
      {
         if i > 0
         {
            ui.separator();
         }
         if let Some(action) = device_row(ui, device)
         {
            command = Some(action);
         }
      }
   });

   command
}

fn device_row(ui: &mut egui::Ui, device: &DeviceRow) -> Option<Command>
{
   let mut command = None;
   let fitness = device.kind != DeviceKind::Other;

   // Line 1: status dot, name, address, kind chip, RSSI.
   ui.horizontal(|ui| {
      let (rect, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
      let dot = if device.connected { theme::GREEN } else { theme::TEXT_DISABLED };
      ui.painter().circle_filled(rect.center(), 4.0, dot);

      let name_color = match (device.connected, fitness)
      {
         | (true, _)      => theme::TEXT_PRIMARY,
         | (false, true)  => theme::TEXT_SECONDARY,
         | (false, false) => theme::TEXT_DISABLED,
      };
      ui.label(RichText::new(device.display_name()).color(name_color).size(14.0).strong());
      ui.label(theme::mono(device.addr.to_string(), theme::TEXT_DISABLED));
      if fitness
      {
         chip(ui, &device.kind.description().to_uppercase());
      }
      if let Some(rssi) = device.rssi
      {
         ui.label(theme::mono(format!("{rssi} dBm"), theme::TEXT_DIM));
      }
   });

   // Line 2: state summary + running DIRCON service, indented under the dot.
   ui.horizontal(|ui| {
      ui.add_space(20.0);
      let mut states = Vec::new();
      if device.connected
      {
         states.push("connected");
      }
      if device.trusted
      {
         states.push("trusted");
      }
      if device.paired
      {
         states.push("paired");
      }
      if !fitness
      {
         states.push("not bridged (no fitness service)");
      }
      if !states.is_empty()
      {
         ui.label(RichText::new(states.join(" · ")).color(theme::TEXT_DIM).size(11.5));
      }
      if let Some(bridged) = &device.bridged
      {
         ui.label(RichText::new(format!("DIRCON: {} :{}", bridged.instance, bridged.port))
                     .color(theme::GREEN)
                     .size(11.5));
      }
   });

   // Line 3: actions (or the in-flight operation).
   ui.horizontal(|ui| {
      ui.add_space(20.0);
      if let Some(busy) = device.busy
      {
         ui.add(egui::Spinner::new().size(12.0));
         ui.label(RichText::new(busy).color(theme::TEXT_DIM).size(11.5));
         return;
      }

      if device.connected
      {
         let disconnect =
            egui::Button::new(RichText::new("Disconnect").color(theme::RED_SOFT).size(12.0))
               .stroke(egui::Stroke::new(1.0, theme::RED_BORDER));
         if ui.add(disconnect).clicked()
         {
            command = Some(Command::Disconnect(device.addr));
         }
      }
      else
      {
         let connect =
            egui::Button::new(RichText::new("Connect").color(theme::ACCENT_TEXT).size(12.0).strong())
               .fill(theme::ACCENT);
         if ui.add(connect).clicked()
         {
            command = Some(Command::Connect(device.addr));
         }
      }

      let mut trusted = device.trusted;
      if ui.checkbox(&mut trusted, RichText::new("Trust").color(theme::TEXT_SECONDARY).size(12.0))
           .changed()
      {
         command = Some(Command::SetTrusted(device.addr, trusted));
      }

      if !device.paired
         && ui.add(egui::Button::new(RichText::new("Pair").color(theme::TEXT_DIM).size(11.5))
                      .stroke(egui::Stroke::new(1.0, theme::BORDER_DIM)))
              .on_hover_text("Bond with the device. Fitness devices normally need only \
                              Connect; use this for devices that refuse unbonded connections.")
              .clicked()
      {
         command = Some(Command::Pair(device.addr));
      }

      // Forget behind a click-to-confirm menu so a stray click can't unpair a device.
      ui.menu_button(RichText::new("Forget…").color(theme::TEXT_DIM).size(11.5), |ui| {
           ui.label(RichText::new("Remove pairing and cached data from BlueZ?")
                       .color(theme::TEXT_SECONDARY)
                       .size(12.0));
           let confirm =
              egui::Button::new(RichText::new("Forget device").color(theme::RED_SOFT).size(12.0))
                 .stroke(egui::Stroke::new(1.0, theme::RED_BORDER));
           if ui.add(confirm).clicked()
           {
              command = Some(Command::Forget(device.addr));
              ui.close();
           }
        });
   });

   command
}

/// Small uppercase pill label (e.g. "SMART TRAINER").
fn chip(ui: &mut egui::Ui, text: &str)
{
   egui::Frame::new().fill(theme::BG_CHIP)
                     .corner_radius(10)
                     .inner_margin(egui::Margin::symmetric(7, 2))
                     .show(ui, |ui| {
                        ui.label(RichText::new(text).color(theme::TEXT_DIM).size(9.5).strong());
                     });
}
