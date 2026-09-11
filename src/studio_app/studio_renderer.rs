//! The dashboard draws with Direct3D 12, through wgpu.
//!
//! Direct3D is Windows' own graphics API. OpenGL, which the dashboard used
//! before, depends on the graphics vendor's driver: in a virtual machine or a
//! Remote Desktop session without GPU support, Windows offers only OpenGL 1.1
//! and the dashboard could not start. Direct3D 12 is always available on
//! Windows 10 and 11, if need be through WARP, Windows' software adapter.
//!
//! Everything wgpu would otherwise take from the environment is fixed here.
//! Its default shader-compiler setting loads `dxcompiler.dll` from anywhere on
//! the PATH when one is present, and environment variables can point it at
//! other compiler or Direct3D runtime DLLs. The dashboard uses only what ships
//! with Windows: Direct3D 12 and the FXC compiler (`d3dcompiler_47.dll`).

use std::sync::Arc;

use eframe::egui_wgpu::{wgpu, WgpuConfiguration, WgpuSetup, WgpuSetupCreateNew};

pub(super) fn wgpu_configuration() -> WgpuConfiguration {
    WgpuConfiguration {
        wgpu_setup: WgpuSetup::CreateNew(WgpuSetupCreateNew {
            instance_descriptor: wgpu::InstanceDescriptor {
                backends: wgpu::Backends::DX12,
                flags: wgpu::InstanceFlags::from_build_config(),
                backend_options: wgpu::BackendOptions {
                    dx12: wgpu::Dx12BackendOptions {
                        shader_compiler: wgpu::Dx12Compiler::Fxc,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
                display: None,
            },
            native_adapter_selector: Some(Arc::new(choose_adapter)),
            device_descriptor: Arc::new(|_adapter| wgpu::DeviceDescriptor {
                label: Some("dashboard device"),
                required_limits: wgpu::Limits {
                    // As egui's default: a surface as large as a 4K-plus window.
                    max_texture_dimension_2d: 8192,
                    ..wgpu::Limits::default()
                },
                // A settings window, not a game: favour memory over throughput.
                memory_hints: wgpu::MemoryHints::MemoryUsage,
                ..Default::default()
            }),
            ..WgpuSetupCreateNew::without_display_handle()
        }),
        ..Default::default()
    }
}

/// The first adapter able to draw to the dashboard window, keeping Windows'
/// own order, which lists the GPU driving the main display first. WARP, the
/// software adapter, is used only when no hardware adapter can.
fn choose_adapter(
    adapters: &[wgpu::Adapter],
    surface: Option<&wgpu::Surface<'_>>,
) -> Result<wgpu::Adapter, String> {
    let candidates: Vec<(wgpu::DeviceType, bool)> = adapters
        .iter()
        .map(|adapter| {
            let presentable = surface.is_none_or(|surface| adapter.is_surface_supported(surface));
            (adapter.get_info().device_type, presentable)
        })
        .collect();
    let Some(index) = preferred_adapter(&candidates) else {
        let found: Vec<String> = adapters
            .iter()
            .map(|adapter| {
                let info = adapter.get_info();
                format!("{} ({:?})", info.name, info.device_type)
            })
            .collect();
        return Err(format!(
            "no Direct3D 12 adapter can draw the dashboard window; found: {}",
            if found.is_empty() {
                "none".to_string()
            } else {
                found.join(", ")
            }
        ));
    };
    let info = adapters[index].get_info();
    crate::diagnose::log(format!(
        "dashboard renderer: Direct3D 12 on {} ({:?}, driver {} {})",
        info.name, info.device_type, info.driver, info.driver_info
    ));
    Ok(adapters[index].clone())
}

/// Index of the adapter to use, from each adapter's kind and whether it can
/// present to the window: the first presentable hardware adapter, else the
/// first presentable software one.
fn preferred_adapter(candidates: &[(wgpu::DeviceType, bool)]) -> Option<usize> {
    candidates
        .iter()
        .enumerate()
        .filter(|(_, (_, presentable))| *presentable)
        .min_by_key(|(index, (kind, _))| (*kind == wgpu::DeviceType::Cpu, *index))
        .map(|(index, _)| index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wgpu::DeviceType::{Cpu, DiscreteGpu, IntegratedGpu, Other, VirtualGpu};

    #[test]
    fn keeps_windows_order_among_hardware_adapters() {
        assert_eq!(
            preferred_adapter(&[(IntegratedGpu, true), (DiscreteGpu, true)]),
            Some(0)
        );
        assert_eq!(
            preferred_adapter(&[(DiscreteGpu, true), (IntegratedGpu, true)]),
            Some(0)
        );
    }

    #[test]
    fn software_rendering_is_the_last_resort() {
        assert_eq!(
            preferred_adapter(&[(Cpu, true), (VirtualGpu, true)]),
            Some(1)
        );
        assert_eq!(preferred_adapter(&[(Cpu, true), (Other, true)]), Some(1));
        // A VM without a GPU driver offers only WARP.
        assert_eq!(preferred_adapter(&[(Cpu, true)]), Some(0));
    }

    #[test]
    fn adapters_that_cannot_draw_to_the_window_are_skipped() {
        assert_eq!(
            preferred_adapter(&[(DiscreteGpu, false), (Cpu, true)]),
            Some(1)
        );
        assert_eq!(preferred_adapter(&[(DiscreteGpu, false)]), None);
        assert_eq!(preferred_adapter(&[]), None);
    }
}
