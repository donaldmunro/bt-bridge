use std::collections::VecDeque;

use eframe::egui::{self, RichText};

use crate::{shared::{LogLevel, LogLine, Shared, SharedState},
            ui::theme};

pub struct LogOutput
{
   /// The user clicked "Save…": the formatted buffer to write. The caller must open
   /// the file dialog *after* releasing the shared-state lock (see `save_to_file`).
   pub save: Option<String>,
}

pub fn show(ui: &mut egui::Ui, state: &mut SharedState) -> LogOutput
{
   let mut out = LogOutput { save: None };

   ui.horizontal(|ui| {
      ui.label(theme::section_header("Log"));
      ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
         if ui.link(RichText::new("Clear").color(theme::TEXT_DIM).size(11.5)).clicked()
         {
            state.logs.clear();
         }
         if ui.link(RichText::new("Copy").color(theme::TEXT_DIM).size(11.5)).clicked()
         {
            ui.ctx().copy_text(format_buffer(&state.logs));
         }
         if ui.link(RichText::new("Save…").color(theme::TEXT_DIM).size(11.5)).clicked()
         {
            out.save = Some(format_buffer(&state.logs));
         }
      });
   });
   ui.separator();

   egui::ScrollArea::vertical().auto_shrink([false, false])
                               .stick_to_bottom(true)
                               .id_salt("log")
                               .show(ui, |ui| {
                                  for line in &state.logs
                                  {
                                     ui.horizontal_wrapped(|ui| {
                                        ui.spacing_mut().item_spacing.x = 6.0;
                                        ui.label(theme::mono(&line.time, theme::TEXT_FAINT));
                                        ui.label(RichText::new(level_tag(line.level))
                                                    .monospace()
                                                    .color(level_color(line.level))
                                                    .size(12.0)
                                                    .strong());
                                        ui.label(theme::mono(&line.message,
                                                             theme::TEXT_SECONDARY));
                                     });
                                  }
                               });

   out
}

/// Ask for a destination and write the formatted buffer, reporting the outcome as a
/// log line. Call *without* the shared-state lock held: the native dialog blocks this
/// thread until dismissed, and the controller task and the tracing capture layer both
/// take that lock in the meantime.
pub fn save_to_file(shared: &Shared, text: &str)
{
   let Some(path) = rfd::FileDialog::new().set_title("Save log")
                                          .set_file_name("bt-bridge.log")
                                          .save_file()
   else
   {
      return;
   };

   let line = match std::fs::write(&path, text)
   {
      | Ok(()) => LogLine { time:    timestamp(),
                            level:   LogLevel::Info,
                            message: format!("log saved to {}", path.display()) },
      | Err(e) => LogLine { time:    timestamp(),
                            level:   LogLevel::Error,
                            message: format!("cannot save log to {}: {e}", path.display()) },
   };
   shared.lock().unwrap().push_log(line);
}

/// The plain-text form used by both Copy and Save.
fn format_buffer(logs: &VecDeque<LogLine>) -> String
{
   logs.iter().map(|l| format!("{} {} {}\n", l.time, level_tag(l.level), l.message)).collect()
}

/// Same timestamp format as `tracing_capture`, so saved/exported lines stay uniform.
fn timestamp() -> String
{
   chrono::Local::now().format("%H:%M:%S%.3f").to_string()
}

fn level_tag(level: LogLevel) -> &'static str
{
   match level
   {
      | LogLevel::Debug => "DEBUG",
      | LogLevel::Info => "INFO ",
      | LogLevel::Warn => "WARN ",
      | LogLevel::Error => "ERROR",
   }
}

fn level_color(level: LogLevel) -> egui::Color32
{
   match level
   {
      | LogLevel::Debug => theme::TEXT_DISABLED,
      | LogLevel::Info => theme::LOG_INFO,
      | LogLevel::Warn => theme::AMBER,
      | LogLevel::Error => theme::RED,
   }
}
