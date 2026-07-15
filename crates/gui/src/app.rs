use std::time::Duration;

use eframe::egui::{self, RichText};
use tokio::sync::mpsc::UnboundedSender;

use crate::{shared::{Command, Shared},
            ui::{self, theme, toolbar::ToolbarAction}};

pub struct App
{
   shared: Shared,
   tx:     UnboundedSender<Command>,
   /// Owns the tokio runtime so the controller outlives the whole app.
   _runtime: tokio::runtime::Runtime,
}

impl App
{
   pub fn new(cc: &eframe::CreationContext<'_>, shared: Shared, tx: UnboundedSender<Command>,
              rx: tokio::sync::mpsc::UnboundedReceiver<Command>,
              runtime: tokio::runtime::Runtime)
              -> Self
   {
      theme::apply(&cc.egui_ctx);
      runtime.spawn(crate::controller::run(shared.clone(), cc.egui_ctx.clone(), rx));
      Self { shared, tx, _runtime: runtime }
   }
}

impl eframe::App for App
{
   fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame)
   {
      // Log lines can arrive from tasks that don't request repaints (e.g. the serve
      // loop); a coarse fallback keeps the pane fresh without burning CPU.
      ui.ctx().request_repaint_after(Duration::from_millis(500));

      let ctx = ui.ctx().clone();
      let mut log_save_request = None;
      let restart_offer;
      {
         let mut state = self.shared.lock().unwrap();

         egui::Panel::top("toolbar")
            .frame(egui::Frame::new().fill(theme::BG_TOOLBAR)
                                     .inner_margin(egui::Margin::symmetric(14, 10)))
            .show(ui, |ui| {
               if let Some(action) = ui::toolbar::show(ui, &state)
               {
                  let command = match action
                  {
                     | ToolbarAction::Start => Command::Start,
                     | ToolbarAction::Refresh => Command::Refresh,
                     | ToolbarAction::Stop => Command::Stop,
                     | ToolbarAction::SetReconnect(value) => Command::SetReconnect(value),
                     | ToolbarAction::SaveReconnect => Command::SaveReconnect,
                  };
                  let _ = self.tx.send(command);
               }
            });

         egui::Panel::left("devices")
            .resizable(true)
            .default_size(700.0)
            .frame(egui::Frame::new().fill(theme::BG_WINDOW)
                                     .inner_margin(egui::Margin::symmetric(12, 10)))
            .show(ui, |ui| {
               if let Some(command) = ui::devices::show(ui, &state)
               {
                  let _ = self.tx.send(command);
               }
            });

         egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(theme::BG_WINDOW)
                                     .inner_margin(egui::Margin::symmetric(12, 10)))
            .show(ui, |ui| {
               log_save_request = ui::log::show(ui, &mut state).save;
            });

         restart_offer = state.restart_offer;
      }

      if let Some(text) = log_save_request
      {
         // Outside the state-lock scope: the dialog blocks until dismissed.
         ui::log::save_to_file(&self.shared, &text);
      }
      if restart_offer
      {
         self.confirm_restart_modal(&ctx);
      }
   }

   /// Orderly shutdown on window close: stop the bridge (mDNS goodbye, client close)
   /// before the runtime is dropped with the app.
   fn on_exit(&mut self)
   {
      let _ = self.tx.send(Command::Stop);
      std::thread::sleep(Duration::from_millis(400));
   }
}

impl App
{
   /// A device connected while the bridge runs with a training app attached: split-mode
   /// services are created at start, so including it needs a restart that briefly
   /// drops the app's DIRCON connection. Ask first.
   fn confirm_restart_modal(&mut self, ctx: &egui::Context)
   {
      egui::Window::new("Include new device")
         .collapsible(false)
         .resizable(false)
         .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
         .show(ctx, |ui| {
            ui.label(RichText::new("A newly connected device is not part of the running \
                                    bridge. Restarting adds it, but will briefly \
                                    disconnect your training app. Restart now?")
                        .color(theme::TEXT_SECONDARY));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
               let restart = egui::Button::new(RichText::new("Restart")
                                                  .color(theme::ACCENT_TEXT)
                                                  .strong())
                                .fill(theme::ACCENT);
               if ui.add(restart).clicked()
               {
                  let _ = self.tx.send(Command::RestartBridge);
                  self.shared.lock().unwrap().restart_offer = false;
               }
               if ui.button("Not now").clicked()
               {
                  self.shared.lock().unwrap().restart_offer = false;
               }
            });
         });
   }
}
