//! GPU device setup.
use crate::Res;

pub struct Gpu {
    pub adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
}

/// `PROTEUS_BACKEND` = `dx12` (default), `vulkan` or `auto`.
fn backends() -> wgpu::Backends {
    match std::env::var("PROTEUS_BACKEND").unwrap_or_default().to_ascii_lowercase().as_str() {
        "vulkan" => wgpu::Backends::VULKAN,
        "auto" => wgpu::Backends::PRIMARY,
        _ => wgpu::Backends::DX12,
    }
}

pub fn instance() -> wgpu::Instance {
    wgpu::Instance::new(&wgpu::InstanceDescriptor { backends: backends(), ..Default::default() })
}

pub fn init(instance: wgpu::Instance, surface: Option<&wgpu::Surface<'_>>) -> Res<Gpu> {
    let opts = wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::None,
        force_fallback_adapter: false,
        compatible_surface: surface,
    };
    let adapter = match pollster::block_on(instance.request_adapter(&opts)) {
        Ok(a) => a,
        Err(first) => {
            // The preferred backend is unavailable: accept any.
            if surface.is_some() {
                return Err(format!("no GPU adapter for the chosen backend: {first}").into());
            }
            let any = wgpu::Instance::new(&wgpu::InstanceDescriptor { backends: wgpu::Backends::PRIMARY, ..Default::default() });
            pollster::block_on(any.request_adapter(&opts)).map_err(|e| format!("no GPU adapter: {e}"))?
        }
    };
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("proteus"),
        required_features: wgpu::Features::empty(),
        required_limits: wgpu::Limits::default(),
        memory_hints: wgpu::MemoryHints::MemoryUsage,
        ..Default::default()
    }))
    .map_err(|e| format!("GPU device: {e}"))?;
    Ok(Gpu { adapter, device, queue })
}
