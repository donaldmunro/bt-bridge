use eframe::egui::{self, RichText};

use crate::{shared::{DaemonState, SharedState},
            ui::theme};

pub enum ToolbarAction
{
   Start,
   Refresh,
   Stop,
   SetReconnect(bool),
   SaveReconnect,
}

pub fn show(ui: &mut egui::Ui, state: &SharedState) -> Option<ToolbarAction>
{
   let mut action = None;

   ui.horizontal(|ui| {
      ui.label(RichText::new("bt-bridge").color(theme::TEXT_PRIMARY).size(15.0).strong());
      badge(ui, state);

      ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
         let running = matches!(state.daemon_state,
                                DaemonState::Running
                                | DaemonState::Starting
                                | DaemonState::WaitingForDevices);

         let stop = egui::Button::new(RichText::new("Stop").color(theme::RED_SOFT))
                       .stroke(egui::Stroke::new(1.0, theme::RED_BORDER));
         if ui.add_enabled(running, stop).clicked()
         {
            action = Some(ToolbarAction::Stop);
         }

         let refresh = egui::Button::new(RichText::new("Refresh").color(theme::TEXT_SECONDARY))
                          .stroke(egui::Stroke::new(1.0, theme::BORDER));
         if ui.add(refresh).clicked()
         {
            action = Some(ToolbarAction::Refresh);
         }

         let start = egui::Button::new(RichText::new("Start")
                                          .color(theme::ACCENT_TEXT)
                                          .strong())
                        .fill(theme::ACCENT);
         if ui.add_enabled(state.daemon_state == DaemonState::Stopped, start).clicked()
         {
            action = Some(ToolbarAction::Start);
         }

         ui.add_space(10.0);

         // Persist the reconnect choice; only meaningful when it differs from the file.
         let dirty = state.reconnect != state.reconnect_saved;
         let save = egui::Button::new(RichText::new("Save").color(theme::TEXT_SECONDARY))
                       .stroke(egui::Stroke::new(1.0, theme::BORDER));
         if ui.add_enabled(dirty, save)
               .on_hover_text("Save the reconnect setting to the config file")
               .clicked()
         {
            action = Some(ToolbarAction::SaveReconnect);
         }

         let mut reconnect = state.reconnect;
         if ui.checkbox(&mut reconnect,
                        RichText::new("Reconnect").color(theme::TEXT_SECONDARY))
               .on_hover_text("Actively reconnect bridged devices that drop their BLE \
                               link (--reconnect); applies when the bridge starts")
               .changed()
         {
            action = Some(ToolbarAction::SetReconnect(reconnect));
         }
      });
   });

   action
}

fn badge(ui: &mut egui::Ui, state: &SharedState)
{
   let (color, label) = match state.daemon_state
   {
      | DaemonState::Stopped => (theme::TEXT_DISABLED, "Stopped".to_string()),
      | DaemonState::Starting => (theme::AMBER, "Starting…".to_string()),
      | DaemonState::WaitingForDevices => (theme::AMBER, "Waiting for devices".to_string()),
      | DaemonState::Stopping => (theme::AMBER, "Stopping…".to_string()),
      | DaemonState::Running =>
      {
         let services = match state.services
         {
            | 1 => "1 service".to_string(),
            | n => format!("{n} services"),
         };
         let clients = match state.clients
         {
            | 1 => "1 client".to_string(),
            | n => format!("{n} clients"),
         };
         (theme::GREEN, format!("Running · {services} · {clients}"))
      }
   };

   egui::Frame::new().fill(theme::BG_BADGE)
                     .corner_radius(10)
                     .inner_margin(egui::Margin::symmetric(10, 4))
                     .show(ui, |ui| {
                        ui.horizontal(|ui| {
                           ui.spacing_mut().item_spacing.x = 6.0;
                           let (rect, _) =
                              ui.allocate_exact_size(egui::vec2(7.0, 7.0), egui::Sense::hover());
                           ui.painter().circle_filled(rect.center(), 3.5, color);
                           ui.label(RichText::new(label).color(color).size(11.5).strong());
                        });
                     });
}
