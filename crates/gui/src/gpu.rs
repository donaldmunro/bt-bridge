//! Render-adapter (GPU) selection for the egui window.
//!
//! eframe renders through wgpu here. By default wgpu picks an adapter with the
//! high-performance power preference (`WGPU_POWER_PREF` overrides). These helpers
//! implement the `--software`, `--gpu <NAME>` and `--list-gpus` options on top of
//! `egui_wgpu`'s `native_adapter_selector` hook, which receives every enumerated
//! adapter and returns the one to use.

use std::sync::Arc;

use eframe::{egui_wgpu, wgpu};

#[derive(Debug, Clone)]
pub enum GpuChoice
{
   /// wgpu's default heuristic (high-performance GPU, `WGPU_POWER_PREF` overrides).
   Auto,
   /// A software (CPU) rasterizer such as Mesa llvmpipe/lavapipe - no GPU used.
   Software,
   /// The adapter whose name contains this string, case-insensitively.
   Named(String),
}

/// The wgpu configuration for `NativeOptions.wgpu_options` implementing `choice`.
pub fn wgpu_options(choice: GpuChoice) -> egui_wgpu::WgpuConfiguration
{
   let mut setup = egui_wgpu::WgpuSetupCreateNew::without_display_handle();
   setup.native_adapter_selector = match choice
   {
      | GpuChoice::Auto => None,
      | GpuChoice::Software => Some(Arc::new(select_software)),
      | GpuChoice::Named(name) =>
      {
         Some(Arc::new(move |adapters: &[wgpu::Adapter], surface: Option<&wgpu::Surface<'_>>| {
                 select_named(&name, adapters, surface)
              }))
      }
   };
   egui_wgpu::WgpuConfiguration { wgpu_setup: setup.into(), ..Default::default() }
}

/// Print the available render adapters (`--list-gpus`). The same adapter can appear
/// once per graphics backend (Vulkan, OpenGL); any of its names works with `--gpu`.
pub fn print_adapters()
{
   let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle_from_env();
   descriptor.backends = default_backends();
   let instance = wgpu::Instance::new(descriptor);
   let adapters = pollster::block_on(instance.enumerate_adapters(default_backends()));
   if adapters.is_empty()
   {
      println!("no render adapters found");
      return;
   }
   println!("available render adapters (pick with --gpu <NAME>, any substring works):");
   for adapter in &adapters
   {
      let info = adapter.get_info();
      let kind = match info.device_type
      {
         | wgpu::DeviceType::DiscreteGpu => "discrete GPU",
         | wgpu::DeviceType::IntegratedGpu => "integrated GPU",
         | wgpu::DeviceType::VirtualGpu => "virtual GPU",
         | wgpu::DeviceType::Cpu => "CPU (software)",
         | wgpu::DeviceType::Other => "other",
      };
      println!("  {:40} [{kind}, {} backend, driver: {}]", info.name, info.backend,
               if info.driver.is_empty() { "unknown" } else { &info.driver });
   }
}

/// Same backend set `egui_wgpu` enumerates with (`WGPU_BACKEND` overrides), so the
/// listing matches what the selector will be offered.
fn default_backends() -> wgpu::Backends
{
   wgpu::Backends::from_env().unwrap_or(wgpu::Backends::PRIMARY | wgpu::Backends::GL)
}

fn select_software(adapters: &[wgpu::Adapter], surface: Option<&wgpu::Surface<'_>>)
                   -> Result<wgpu::Adapter, String>
{
   adapters.iter()
           .filter(|a| compatible(a, surface))
           .find(|a| a.get_info().device_type == wgpu::DeviceType::Cpu)
           .cloned()
           .ok_or_else(|| {
              format!("no software (CPU) render adapter available - install Mesa's \
                       llvmpipe/lavapipe. Available adapters: {}",
                      names(adapters))
           })
}

fn select_named(name: &str, adapters: &[wgpu::Adapter], surface: Option<&wgpu::Surface<'_>>)
                -> Result<wgpu::Adapter, String>
{
   let wanted = name.to_lowercase();
   adapters.iter()
           .filter(|a| compatible(a, surface))
           .find(|a| a.get_info().name.to_lowercase().contains(&wanted))
           .cloned()
           .ok_or_else(|| {
              format!("no render adapter matches --gpu {name:?}. Available adapters: {}",
                      names(adapters))
           })
}

fn compatible(adapter: &wgpu::Adapter, surface: Option<&wgpu::Surface<'_>>) -> bool
{
   surface.is_none_or(|s| adapter.is_surface_supported(s))
}

fn names(adapters: &[wgpu::Adapter]) -> String
{
   adapters.iter().map(|a| format!("{:?}", a.get_info().name)).collect::<Vec<_>>().join(", ")
}
