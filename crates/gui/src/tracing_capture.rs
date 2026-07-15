//! A `tracing` layer that mirrors the daemon's log lines into the GUI log pane.
//!
//! Only bridge messages are mirrored: the GUI's render stack (eframe, egui, wgpu,
//! winit, ...) also logs through `tracing` and would confuse users with
//! non-Bluetooth noise. The stderr layer in `main.rs` is deliberately not
//! filtered this way - terminal launches still see everything.

use tracing::{Event, Level, Subscriber, field::{Field, Visit}};
use tracing_subscriber::layer::{Context, Layer};

use crate::shared::{LogLevel, LogLine, Shared};

pub struct CaptureLayer
{
   pub shared: Shared,
}

/// Show only events from our own crates: the core daemon plus the GUI's controller
/// (whose start/stop/re-route/config messages are user-facing bridge feedback).
fn bridge_target(target: &str) -> bool
{
   target.starts_with("bt_bridge")
}

#[derive(Default)]
struct LineVisitor
{
   message: String,
   fields:  Vec<(&'static str, String)>,
}

impl Visit for LineVisitor
{
   fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug)
   {
      if field.name() == "message"
      {
         self.message = format!("{value:?}");
      }
      else
      {
         self.fields.push((field.name(), format!("{value:?}")));
      }
   }
}

impl<S: Subscriber> Layer<S> for CaptureLayer
{
   fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>)
   {
      if !bridge_target(event.metadata().target())
      {
         return;
      }

      let level = match *event.metadata().level()
      {
         | Level::ERROR => LogLevel::Error,
         | Level::WARN => LogLevel::Warn,
         | Level::INFO => LogLevel::Info,
         | _ => LogLevel::Debug,
      };

      let mut visitor = LineVisitor::default();
      event.record(&mut visitor);
      let mut message = visitor.message;
      for (name, value) in visitor.fields
      {
         message.push_str(&format!(" {name}={value}"));
      }

      let line = LogLine { time: chrono::Local::now().format("%H:%M:%S%.3f").to_string(),
                           level,
                           message };
      if let Ok(mut state) = self.shared.lock()
      {
         state.push_log(line);
      }
   }
}

#[cfg(test)]
mod tests
{
   use super::bridge_target;

   #[test]
   fn only_bridge_crate_targets_reach_the_log_pane()
   {
      assert!(bridge_target("bt_bridge_core::mdns"));
      assert!(bridge_target("bt_bridge_core::dircon::server"));
      assert!(bridge_target("bt_bridge_gui::controller"));
      assert!(!bridge_target("eframe::native::run"));
      assert!(!bridge_target("egui_wgpu::renderer"));
      assert!(!bridge_target("wgpu_core::device"));
      assert!(!bridge_target("winit::event_loop"));
      assert!(!bridge_target("mdns_sd::service_daemon"));
   }
}
