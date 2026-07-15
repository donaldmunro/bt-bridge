//! bt-bridge-gui: desktop control panel for the bt-bridge daemon.
//! Links bt-bridge-core in-process - no CLI shell-out, no IPC.
//!
//! Design spec: design/design_handoff_bluetooth_device_selector/README.md

mod app;
mod controller;
mod gpu;
mod shared;
mod tracing_capture;
mod ui;

use std::sync::{Arc, Mutex};

use clap::Parser;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

use crate::shared::SharedState;

#[derive(Parser, Debug)]
#[command(name = "bt-bridge-gui",
          about = "Desktop control panel for the bt-bridge daemon")]
struct Args
{
   /// Render with the CPU (rasterizer, e.g. Mesa llvmpipe) instead of a GPU
   #[arg(long, conflicts_with = "gpu")]
   disable_gpu: bool,

   /// Render on the GPU whose name contains NAME, case-insensitive (see --list-gpus)
   #[arg(long, value_name = "NAME")]
   gpu: Option<String>,

   /// List the available GPUs / render adapters and exit
   #[arg(long)]
   list_gpus: bool,
}

fn main() -> eframe::Result
{
   let args = Args::parse();
   if args.list_gpus
   {
      gpu::print_adapters();
      return Ok(());
   }
   let gpu_choice = match (args.disable_gpu, args.gpu)
   {
      | (true, _) => gpu::GpuChoice::Software,
      | (false, Some(name)) => gpu::GpuChoice::Named(name),
      | (false, None) => gpu::GpuChoice::Auto,
   };

   let shared: shared::Shared = Arc::new(Mutex::new(SharedState::new()));

   // Daemon log lines go to the in-app log pane and (for terminal launches) stderr.
   tracing_subscriber::registry()
      .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
      .with(tracing_capture::CaptureLayer { shared: shared.clone() })
      .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
      .init();

   let runtime = tokio::runtime::Runtime::new().expect("cannot start tokio runtime");
   let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

   let options = eframe::NativeOptions {
      viewport: eframe::egui::ViewportBuilder::default().with_inner_size([1400.0, 840.0])
                                                        .with_min_inner_size([960.0, 600.0])
                                                        .with_title("bt-bridge"),
      wgpu_options: gpu::wgpu_options(gpu_choice),
      ..Default::default()
   };
   eframe::run_native("bt-bridge", options,
                      Box::new(move |cc| Ok(Box::new(app::App::new(cc, shared, tx, rx, runtime)))))
}
