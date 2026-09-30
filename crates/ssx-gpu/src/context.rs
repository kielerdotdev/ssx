//! Headless GPU device management: adapter selection, limit negotiation, error scopes,
//! device-loss handling.
//!
//! # Why it looks like this
//!
//! * **Adapter policy.** Discrete GPU, then integrated, then virtual/other, then software
//!   (llvmpipe, WARP). The primary backends (Vulkan/Metal/DX12) are tried first and GL only
//!   when they produce nothing, because the GL backend is the least conformant one.
//!   `SSX_GPU_BACKEND`, `SSX_GPU_ADAPTER` and `SSX_GPU_FALLBACK` override the policy, so a
//!   user with a broken driver can get out of trouble without a rebuild.
//! * **Minimal requirements.** No optional features are requested (in particular not
//!   `shader-f16`: half-float data is uploaded as `Rgba16Float` textures and converted by
//!   the texture unit). Limits start from the downlevel defaults and are raised only where
//!   large images need it (texture size, buffer sizes, dispatch size).
//! * **No panics.** wgpu's default uncaptured-error behaviour is to panic; we replace it
//!   with a logging handler and wrap every operation in error scopes that are turned into
//!   [`GpuError`].
//! * **Device loss.** A lost device flips a flag (set from wgpu's callback). The next call
//!   through [`GpuContext::handle`] builds a fresh device with a new *generation* number;
//!   objects that cache GPU resources compare generations and rebuild lazily.

use std::sync::{
    Arc, Mutex, OnceLock, RwLock,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

use crate::error::{GpuError, Result};

/// Which backends may be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BackendChoice {
    /// Primary backends first, GL only as a last resort.
    #[default]
    Auto,
    /// Vulkan only.
    Vulkan,
    /// Direct3D 12 only.
    Dx12,
    /// Metal only.
    Metal,
    /// OpenGL / GLES only.
    Gl,
}

impl BackendChoice {
    fn backends(self) -> wgpu::Backends {
        match self {
            Self::Auto => wgpu::Backends::PRIMARY,
            Self::Vulkan => wgpu::Backends::VULKAN,
            Self::Dx12 => wgpu::Backends::DX12,
            Self::Metal => wgpu::Backends::METAL,
            Self::Gl => wgpu::Backends::GL,
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "auto" | "all" => Some(Self::Auto),
            "vulkan" | "vk" => Some(Self::Vulkan),
            "dx12" | "d3d12" => Some(Self::Dx12),
            "metal" | "mtl" => Some(Self::Metal),
            "gl" | "gles" | "opengl" => Some(Self::Gl),
            _ => None,
        }
    }
}

/// How to pick an adapter.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GpuOptions {
    /// Backend restriction.
    pub backend: BackendChoice,
    /// Case-insensitive substring of the adapter name, or `#N` for the N-th adapter of the
    /// ranked list. `None` lets the policy decide.
    pub adapter: Option<String>,
    /// Only consider software / fallback adapters (llvmpipe, WARP, `SwiftShader`).
    pub force_fallback_adapter: bool,
    /// Prefer an integrated GPU over a discrete one (laptops on battery).
    pub prefer_low_power: bool,
}

impl GpuOptions {
    /// Options from the environment: `SSX_GPU_BACKEND` (`vulkan|dx12|metal|gl|auto`, falling
    /// back to wgpu's `WGPU_BACKEND`), `SSX_GPU_ADAPTER` and `SSX_GPU_FALLBACK=1`.
    pub fn from_env() -> Self {
        let mut o = Self::default();
        let backend =
            std::env::var("SSX_GPU_BACKEND").ok().or_else(|| std::env::var("WGPU_BACKEND").ok());
        if let Some(b) = backend {
            if let Some(c) = BackendChoice::parse(&b) {
                o.backend = c;
            } else {
                tracing::warn!("ignoring unknown GPU backend `{b}`");
            }
        }
        o.adapter = std::env::var("SSX_GPU_ADAPTER").ok().filter(|s| !s.trim().is_empty());
        o.force_fallback_adapter =
            matches!(std::env::var("SSX_GPU_FALLBACK").as_deref(), Ok("1" | "true"));
        o
    }
}

/// A live device with its queue and the facts callers need about it.
///
/// Obtained from [`GpuContext::handle`]. It is cheap to clone (`Arc`) and stays valid, in
/// the sense of being memory safe, after the device is lost; operations on a lost device
/// fail with [`GpuError::DeviceLost`].
#[derive(Debug)]
pub struct DeviceHandle {
    device: wgpu::Device,
    queue: wgpu::Queue,
    limits: wgpu::Limits,
    info: wgpu::AdapterInfo,
    generation: u64,
    lost: Arc<AtomicBool>,
    lost_reason: Arc<Mutex<String>>,
}

impl DeviceHandle {
    /// The wgpu device.
    pub fn device(&self) -> &wgpu::Device {
        &self.device
    }

    /// The wgpu queue.
    pub fn queue(&self) -> &wgpu::Queue {
        &self.queue
    }

    /// The limits this device was created with.
    pub fn limits(&self) -> &wgpu::Limits {
        &self.limits
    }

    /// Adapter information.
    pub fn info(&self) -> &wgpu::AdapterInfo {
        &self.info
    }

    /// Increases by one every time the context creates a new device.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// `true` once wgpu reported the device as lost.
    pub fn is_lost(&self) -> bool {
        self.lost.load(Ordering::Acquire)
    }

    fn lost_error(&self) -> GpuError {
        let reason = self.lost_reason.lock().map(|r| r.clone()).unwrap_or_default();
        GpuError::DeviceLost(reason)
    }

    /// Runs `f` inside out-of-memory / validation / internal error scopes and converts
    /// anything they catch into a [`GpuError`]. `f` must issue all its wgpu calls from the
    /// calling thread (error scopes are thread local).
    pub(crate) fn scoped<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        if self.is_lost() {
            return Err(self.lost_error());
        }
        let oom = self.device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
        let validation = self.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let internal = self.device.push_error_scope(wgpu::ErrorFilter::Internal);
        let result = f();
        let e_internal = pollster::block_on(internal.pop());
        let e_validation = pollster::block_on(validation.pop());
        let e_oom = pollster::block_on(oom.pop());
        if self.is_lost() {
            return Err(self.lost_error());
        }
        if e_oom.is_some() {
            return Err(GpuError::OutOfMemory);
        }
        if let Some(e) = e_validation {
            return Err(GpuError::Validation(e.to_string()));
        }
        if let Some(e) = e_internal {
            return Err(GpuError::Internal(e.to_string()));
        }
        result
    }
}

struct Shared {
    options: GpuOptions,
    instance: wgpu::Instance,
    current: RwLock<Arc<DeviceHandle>>,
    next_generation: AtomicU64,
    recreate: Mutex<()>,
}

/// A shared, thread-safe handle to a headless GPU device.
///
/// Cloning is cheap. Construct with [`GpuContext::new`] or use the lazily created
/// process-wide [`GpuContext::global`].
#[derive(Clone)]
pub struct GpuContext {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for GpuContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let h = self.current();
        f.debug_struct("GpuContext")
            .field("adapter", &h.info.name)
            .field("backend", &h.info.backend)
            .field("device_type", &h.info.device_type)
            .field("generation", &h.generation)
            .field("lost", &h.is_lost())
            .finish()
    }
}

static GLOBAL: OnceLock<GpuContext> = OnceLock::new();

impl GpuContext {
    /// Creates a context with the adapter chosen by `options`.
    pub fn new(options: &GpuOptions) -> Result<Self> {
        let instance = new_instance(options.backend.backends());
        let handle = create_device(&instance, options, 0)?;
        Ok(Self {
            shared: Arc::new(Shared {
                options: options.clone(),
                instance,
                current: RwLock::new(Arc::new(handle)),
                next_generation: AtomicU64::new(1),
                recreate: Mutex::new(()),
            }),
        })
    }

    /// Creates a context using [`GpuOptions::from_env`].
    pub fn new_default() -> Result<Self> {
        Self::new(&GpuOptions::from_env())
    }

    /// The process-wide context, created on first use with [`GpuOptions::from_env`].
    ///
    /// A failed initialisation is not cached, so a later call can succeed (for instance
    /// after a driver was installed).
    pub fn global() -> Result<&'static GpuContext> {
        if let Some(c) = GLOBAL.get() {
            return Ok(c);
        }
        let ctx = Self::new_default()?;
        Ok(GLOBAL.get_or_init(|| ctx))
    }

    fn current(&self) -> Arc<DeviceHandle> {
        match self.shared.current.read() {
            Ok(g) => Arc::clone(&g),
            Err(p) => Arc::clone(&p.into_inner()),
        }
    }

    /// The current device, recreating it first if it was lost.
    pub fn handle(&self) -> Result<Arc<DeviceHandle>> {
        let cur = self.current();
        if !cur.is_lost() {
            return Ok(cur);
        }
        self.recreate_if_stale(cur.generation)
    }

    fn recreate_if_stale(&self, seen_generation: u64) -> Result<Arc<DeviceHandle>> {
        let _guard = self.shared.recreate.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        // Another thread may have recreated it while we waited for the lock.
        let cur = self.current();
        if cur.generation != seen_generation {
            return Ok(cur);
        }
        let generation = self.shared.next_generation.fetch_add(1, Ordering::Relaxed);
        tracing::warn!(generation, "recreating GPU device");
        let fresh =
            Arc::new(create_device(&self.shared.instance, &self.shared.options, generation)?);
        match self.shared.current.write() {
            Ok(mut g) => *g = Arc::clone(&fresh),
            Err(p) => *p.into_inner() = Arc::clone(&fresh),
        }
        Ok(fresh)
    }

    /// Discards the current device and creates a new one (also useful after
    /// [`GpuError::Internal`]).
    pub fn recreate(&self) -> Result<Arc<DeviceHandle>> {
        let cur = self.current();
        cur.device.destroy();
        cur.lost.store(true, Ordering::Release);
        self.recreate_if_stale(cur.generation)
    }

    /// Destroys the current device, as a driver reset would. The next [`handle`] call
    /// recreates it. Intended for tests and for recovery paths.
    ///
    /// [`handle`]: GpuContext::handle
    pub fn simulate_device_loss(&self) {
        let cur = self.current();
        cur.device.destroy();
        // The wgpu callback also sets this, but it may be delivered later.
        if let Ok(mut r) = cur.lost_reason.lock() {
            "device destroyed".clone_into(&mut r);
        }
        cur.lost.store(true, Ordering::Release);
    }

    /// `true` if the current device is lost (and will be recreated on next use).
    pub fn is_lost(&self) -> bool {
        self.current().is_lost()
    }

    /// Generation of the current device.
    pub fn generation(&self) -> u64 {
        self.current().generation
    }

    /// Information about the adapter in use.
    pub fn adapter_info(&self) -> wgpu::AdapterInfo {
        self.current().info.clone()
    }

    /// Limits of the current device.
    pub fn limits(&self) -> wgpu::Limits {
        self.current().limits.clone()
    }

    /// `true` if the adapter is a software rasteriser.
    pub fn is_software(&self) -> bool {
        self.current().info.device_type == wgpu::DeviceType::Cpu
    }
}

fn new_instance(backends: wgpu::Backends) -> wgpu::Instance {
    let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
    desc.backends = backends;
    wgpu::Instance::new(desc)
}

/// Sort key: lower is better.
fn rank(info: &wgpu::AdapterInfo, low_power: bool) -> (u8, u8) {
    use wgpu::DeviceType as T;
    let ty = match (info.device_type, low_power) {
        (T::DiscreteGpu, false) | (T::IntegratedGpu, true) => 0,
        (T::IntegratedGpu, false) | (T::DiscreteGpu, true) => 1,
        (T::VirtualGpu, _) => 2,
        (T::Other, _) => 3,
        (T::Cpu, _) => 4,
    };
    let be = match info.backend {
        wgpu::Backend::Dx12 if cfg!(windows) => 0,
        wgpu::Backend::Metal | wgpu::Backend::Vulkan => 0,
        wgpu::Backend::Dx12 => 1,
        _ => 2,
    };
    (ty, be)
}

fn required_limits(adapter: &wgpu::Adapter) -> wgpu::Limits {
    let a = adapter.limits();
    let mut l = wgpu::Limits::downlevel_defaults().using_resolution(a.clone());
    // Large images: tiling keeps working sets small, but the bigger these are, the fewer
    // tiles are needed. Never ask for more than the adapter offers.
    l.max_buffer_size = a.max_buffer_size;
    l.max_storage_buffer_binding_size = a.max_storage_buffer_binding_size;
    l.max_compute_workgroups_per_dimension = a.max_compute_workgroups_per_dimension;
    l
}

fn create_device(
    instance: &wgpu::Instance,
    options: &GpuOptions,
    generation: u64,
) -> Result<DeviceHandle> {
    let mut adapters = pollster::block_on(instance.enumerate_adapters(options.backend.backends()));
    if adapters.is_empty() && options.backend == BackendChoice::Auto {
        adapters = pollster::block_on(instance.enumerate_adapters(wgpu::Backends::GL));
    }
    let considered = adapters.len();
    adapters.retain(|a| {
        let info = a.get_info();
        a.get_downlevel_capabilities().flags.contains(wgpu::DownlevelFlags::COMPUTE_SHADERS)
            && (!options.force_fallback_adapter || info.device_type == wgpu::DeviceType::Cpu)
    });
    adapters.sort_by_key(|a| rank(&a.get_info(), options.prefer_low_power));
    if let Some(sel) = options.adapter.as_deref() {
        let sel = sel.trim();
        if let Some(idx) = sel.strip_prefix('#').and_then(|n| n.parse::<usize>().ok()) {
            let picked = (idx < adapters.len()).then(|| adapters.swap_remove(idx));
            adapters = picked.into_iter().collect();
        } else {
            let needle = sel.to_lowercase();
            adapters.retain(|a| a.get_info().name.to_lowercase().contains(&needle));
        }
    }
    if adapters.is_empty() {
        return Err(GpuError::NoAdapter(format!(
            "{considered} adapter(s) enumerated for {:?}, none with compute support matching the \
             options {options:?}",
            options.backend
        )));
    }

    let mut last_err = None;
    for adapter in adapters {
        let info = adapter.get_info();
        let desc = wgpu::DeviceDescriptor {
            label: Some("ssx-gpu"),
            required_features: wgpu::Features::empty(),
            required_limits: required_limits(&adapter),
            ..Default::default()
        };
        let limits = desc.required_limits.clone();
        match pollster::block_on(adapter.request_device(&desc)) {
            Ok((device, queue)) => {
                tracing::info!(
                    adapter = %info.name, backend = ?info.backend, kind = ?info.device_type,
                    generation, "GPU device created"
                );
                let lost = Arc::new(AtomicBool::new(false));
                let lost_reason = Arc::new(Mutex::new(String::new()));
                {
                    let lost = Arc::clone(&lost);
                    let reason = Arc::clone(&lost_reason);
                    device.set_device_lost_callback(move |why, msg| {
                        tracing::warn!(?why, %msg, "GPU device lost");
                        if let Ok(mut r) = reason.lock() {
                            *r = format!("{why:?}: {msg}");
                        }
                        lost.store(true, Ordering::Release);
                    });
                }
                device.on_uncaptured_error(Arc::new(|e: wgpu::Error| {
                    tracing::error!("uncaptured GPU error: {e}");
                }));
                return Ok(DeviceHandle {
                    device,
                    queue,
                    limits,
                    info,
                    generation,
                    lost,
                    lost_reason,
                });
            }
            Err(e) => {
                tracing::warn!(adapter = %info.name, "device request failed: {e}");
                last_err = Some(e);
            }
        }
    }
    Err(last_err.map_or_else(|| GpuError::NoAdapter("no adapter".into()), GpuError::DeviceRequest))
}
