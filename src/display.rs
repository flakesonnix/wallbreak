use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail, ensure};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, SurfaceData},
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, delegate_shm,
    output::{OutputHandler, OutputState},
    reexports::{calloop::EventLoop, calloop_wayland_source::WaylandSource},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    shell::{
        WaylandSurface,
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
    },
    shm::{
        Shm, ShmHandler,
        slot::{Buffer, SlotPool},
    },
};
use wayland_client::{
    Connection, Proxy, QueueHandle,
    globals::registry_queue_init,
    protocol::{wl_output, wl_region, wl_shm, wl_surface},
};

use crate::{
    media::{MediaWorker, PlaybackOptions, frame_len},
    project::{Media, MediaKind},
};

const STOP_POLL_INTERVAL: Duration = Duration::from_millis(100);
const MAX_BUFFERS: usize = 3;

#[derive(Debug, serde::Serialize)]
pub struct OutputInfo {
    // Empty only when the compositor supplies no output name; not a selectable alias.
    pub name: String,
    pub description: Option<String>,
}

/// Enumerate outputs without binding layer-shell or creating any surfaces.
pub fn list_outputs() -> Result<Vec<OutputInfo>> {
    let connection = connect()?;
    let (globals, mut queue) = registry_queue_init(&connection)
        .context("could not enumerate the Wayland compositor's globals")?;
    let qh = queue.handle();
    let mut state = OutputList {
        registry: RegistryState::new(&globals),
        outputs: OutputState::new(&globals, &qh),
    };
    queue
        .roundtrip(&mut state)
        .context("could not read Wayland output information")?;
    state
        .outputs
        .outputs()
        .map(|output| {
            let info = state
                .outputs
                .info(&output)
                .context("the compositor has not supplied complete output information; retry")?;
            Ok(OutputInfo {
                name: info.name.unwrap_or_default(),
                description: info.description,
            })
        })
        .collect()
}

pub fn run(
    media: Media,
    options: PlaybackOptions,
    output: Option<String>,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    ensure!(options.fps > 0, "playback FPS must be greater than zero");
    let deadline = options
        .duration
        .map(|duration| {
            Instant::now()
                .checked_add(duration)
                .context("playback duration exceeds the monotonic clock's range")
        })
        .transpose()?;
    let stopped =
        || stop.load(Ordering::Relaxed) || deadline.is_some_and(|end| Instant::now() >= end);
    if stopped() {
        return Ok(());
    }

    let connection = connect()?;
    let (globals, queue) = registry_queue_init(&connection)
        .context("could not enumerate the Wayland compositor's globals")?;
    let qh = queue.handle();
    let compositor = CompositorState::bind(&globals, &qh)
        .context("wl_compositor is unavailable; run inside a Wayland session such as Sway")?;
    ensure!(
        compositor.wl_compositor().version() >= 4,
        "wallpaper rendering requires wl_compositor version 4 or newer; use a current Sway compositor"
    );
    let layer_shell = LayerShell::bind(&globals, &qh).context(
        "zwlr_layer_shell_v1 is unavailable; use Sway or another wlr-layer-shell compositor (not X11)",
    )?;
    let shm = Shm::bind(&globals, &qh)
        .context("wl_shm is unavailable; this wallpaper backend requires shared-memory buffers")?;
    let mut state = Wallpaper {
        catalog: OutputList {
            registry: RegistryState::new(&globals),
            outputs: OutputState::new(&globals, &qh),
        },
        shm,
        selected: None,
        layer: None,
        logical_size: None,
        scale: 1,
        size: None,
        worker: None,
        buffers: Vec::with_capacity(MAX_BUFFERS),
        pending_frame: None,
        received_frame: false,
        callback_pending: false,
        video: matches!(media.kind, MediaKind::Video | MediaKind::Scene),
        poll_interval: (Duration::from_secs(1) / options.fps)
            .clamp(Duration::from_millis(1), STOP_POLL_INTERVAL),
        next_poll: Instant::now(),
        closed: false,
        failure: None,
    };
    let mut event_loop: EventLoop<Wallpaper> =
        EventLoop::try_new().context("could not initialize the Wayland event loop")?;
    WaylandSource::new(connection.clone(), queue)
        .insert(event_loop.handle())
        .map_err(|error| {
            anyhow!(
                "could not register the Wayland event queue: {}",
                error.error
            )
        })?;

    let result = (|| {
        // OutputState waits for both wl_output and optional xdg-output information.
        // Unlike a blocking roundtrip, this wait also observes cancellation.
        while state
            .catalog
            .outputs
            .outputs()
            .any(|item| state.catalog.outputs.info(&item).is_none())
        {
            if stopped() {
                return Ok(());
            }
            event_loop
                .dispatch(STOP_POLL_INTERVAL, &mut state)
                .context("Wayland disconnected while enumerating outputs")?;
        }
        let outputs: Vec<_> = state.catalog.outputs.outputs().collect();
        let infos: Vec<_> = outputs
            .iter()
            .map(|item| {
                state
                    .catalog
                    .outputs
                    .info(item)
                    .context("Wayland output disappeared before selection")
            })
            .collect::<Result<_>>()?;
        let names: Vec<_> = infos.iter().map(|info| info.name.as_deref()).collect();
        let index = select_output(&names, output.as_deref())?;
        if stopped() {
            return Ok(());
        }
        state.scale = infos[index].scale_factor;
        ensure!(
            state.scale > 0,
            "compositor advertised an invalid output scale"
        );
        state.selected = Some(outputs[index].clone());

        let surface = compositor.create_surface_with_data(&qh, SurfaceData::new(None, state.scale));
        let region = compositor.wl_compositor().create_region(&qh, ());
        surface.set_input_region(Some(&region));
        region.destroy();
        let layer = layer_shell.create_layer_surface(
            &qh,
            surface,
            Layer::Background,
            Some("wallbreak"),
            Some(&outputs[index]),
        );
        layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        // Ignore panels' exclusive zones, but reserve no workspace of our own.
        layer.set_exclusive_zone(-1);
        layer.set_size(0, 0);
        layer.commit();
        state.layer = Some(layer);

        loop {
            if let Some(error) = state.failure.take() {
                return Err(error);
            }
            if stopped() || state.closed {
                return Ok(());
            }
            state.render(&media, &options, &qh)?;
            let mut timeout = STOP_POLL_INTERVAL;
            if state.worker.is_some() && (state.video || !state.received_frame) {
                timeout = timeout.min(state.next_poll.saturating_duration_since(Instant::now()));
            }
            if let Some(end) = deadline {
                timeout = timeout.min(end.saturating_duration_since(Instant::now()));
            }
            event_loop
                .dispatch(timeout, &mut state)
                .context("Wayland compositor disconnected or rejected the wallpaper surface")?;
        }
    })();

    // Always unmap and reap the decoder, including render/dispatch failures.
    state.layer = None;
    let cleanup = state.shutdown_worker();
    drop(state);
    let _ = connection.flush();
    match (result, cleanup) {
        (Err(error), Err(cleanup)) => {
            Err(error.context(format!("decoder cleanup also failed: {cleanup:#}")))
        }
        (Err(error), Ok(())) => Err(error),
        (Ok(()), cleanup) => cleanup,
    }
}

fn connect() -> Result<Connection> {
    Connection::connect_to_env().context(
        "could not connect to Wayland; run in the intended Sway session and check WAYLAND_DISPLAY and XDG_RUNTIME_DIR",
    )
}

fn select_output(names: &[Option<&str>], requested: Option<&str>) -> Result<usize> {
    ensure!(
        !names.is_empty(),
        "the Wayland compositor advertises no outputs"
    );
    let Some(name) = requested else {
        return Ok(0);
    };
    let mut matches = names
        .iter()
        .enumerate()
        .filter(|(_, candidate)| **candidate == Some(name));
    let index = matches.next().map(|(index, _)| index).with_context(|| {
        let available = names
            .iter()
            .filter_map(|name| *name)
            .collect::<Vec<_>>()
            .join(", ");
        format!("Wayland output {name:?} was not found; advertised names: [{available}]")
    })?;
    ensure!(
        matches.next().is_none(),
        "the compositor advertised duplicate output name {name:?}; output selection is ambiguous"
    );
    Ok(index)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FrameSize {
    width: u32,
    height: u32,
    stride: i32,
    bytes: usize,
    scale: i32,
}

impl FrameSize {
    fn new(logical: (u32, u32), scale: i32) -> Result<Self> {
        ensure!(
            logical.0 > 0 && logical.1 > 0,
            "compositor configured a zero wallpaper size"
        );
        let factor = u32::try_from(scale).context("compositor supplied a negative buffer scale")?;
        ensure!(factor > 0, "compositor supplied a zero buffer scale");
        let width = logical
            .0
            .checked_mul(factor)
            .context("scaled wallpaper width overflow")?;
        let height = logical
            .1
            .checked_mul(factor)
            .context("scaled wallpaper height overflow")?;
        i32::try_from(width).context("wallpaper width exceeds the Wayland buffer limit")?;
        i32::try_from(height).context("wallpaper height exceeds the Wayland buffer limit")?;
        let stride = width
            .checked_mul(4)
            .and_then(|stride| i32::try_from(stride).ok())
            .context("wallpaper stride exceeds the Wayland buffer limit")?;
        let bytes =
            frame_len(width, height).context("scaled wallpaper framebuffer is too large")?;
        Ok(Self {
            width,
            height,
            stride,
            bytes,
            scale,
        })
    }
}

struct RenderBuffer {
    pool: SlotPool,
    buffer: Buffer,
    size: FrameSize,
}

impl RenderBuffer {
    fn new(size: FrameSize, shm: &Shm) -> Result<Self> {
        // SlotPool rounds allocations up to 64 bytes. Preallocate that exact
        // amount so its automatic doubling cannot inflate a framebuffer pool.
        let capacity = size
            .bytes
            .checked_add(63)
            .context("shared-memory slot size overflow")?
            & !63;
        let mut pool =
            SlotPool::new(capacity, shm).context("could not allocate wallpaper shared memory")?;
        let (buffer, _) = pool
            .create_buffer(
                size.width as i32,
                size.height as i32,
                size.stride,
                wl_shm::Format::Xrgb8888,
            )
            .context("could not create the Wayland wallpaper buffer")?;
        Ok(Self { pool, buffer, size })
    }
}

struct OutputList {
    registry: RegistryState,
    outputs: OutputState,
}

impl OutputHandler for OutputList {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.outputs
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl ProvidesRegistryState for OutputList {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState];
}

delegate_output!(OutputList);
delegate_registry!(OutputList);

struct Wallpaper {
    catalog: OutputList,
    shm: Shm,
    selected: Option<wl_output::WlOutput>,
    layer: Option<LayerSurface>,
    logical_size: Option<(u32, u32)>,
    scale: i32,
    size: Option<FrameSize>,
    worker: Option<MediaWorker>,
    buffers: Vec<RenderBuffer>,
    pending_frame: Option<Vec<u8>>,
    received_frame: bool,
    callback_pending: bool,
    video: bool,
    poll_interval: Duration,
    next_poll: Instant,
    closed: bool,
    failure: Option<anyhow::Error>,
}

impl Wallpaper {
    fn shutdown_worker(&mut self) -> Result<()> {
        if let Some(worker) = self.worker.as_mut() {
            worker
                .shutdown()
                .context("could not stop and reap the media decoder")?;
        }
        self.worker = None;
        Ok(())
    }

    fn render(
        &mut self,
        media: &Media,
        options: &PlaybackOptions,
        qh: &QueueHandle<Self>,
    ) -> Result<()> {
        let Some(logical) = self.logical_size else {
            return Ok(());
        };
        let size = FrameSize::new(logical, self.scale)?;
        if self.size != Some(size) {
            self.shutdown_worker()?;
            self.pending_frame = None;
            self.received_frame = false;
            self.worker = Some(
                MediaWorker::start(media, size.width, size.height, options)
                    .context("could not start wallpaper decoding at the configured output size")?,
            );
            self.size = Some(size);
            self.next_poll = Instant::now();
        }

        let now = Instant::now();
        if now >= self.next_poll && (self.video || !self.received_frame) {
            if let Some(worker) = self.worker.as_mut() {
                if let Some(frame) = worker.take_frame().context("wallpaper decoding failed")? {
                    let frame = frame.into_bgra()?;
                    ensure!(
                        frame.len() == size.bytes,
                        "media decoder returned {} bytes; the {}x{} wallpaper requires {}",
                        frame.len(),
                        size.width,
                        size.height,
                        size.bytes
                    );
                    self.pending_frame = Some(frame);
                    self.received_frame = true;
                }
            }
            self.next_poll = now
                .checked_add(self.poll_interval)
                .context("frame clock overflow")?;
        }

        // A completed callback is permission to draw, not a guarantee that
        // decoding is ready. Keep that permission until a later poll has data.
        if self.callback_pending || self.pending_frame.is_none() {
            return Ok(());
        }
        let available = self
            .buffers
            .iter()
            .position(|item| !item.buffer.slot().has_active_buffers());
        let index = match available {
            Some(index) if self.buffers[index].size == size => index,
            other => {
                if let Some(index) = other {
                    // Only retire released buffers, even across output resizes.
                    self.buffers.swap_remove(index);
                } else if self.buffers.len() == MAX_BUFFERS {
                    return Ok(());
                }
                self.buffers.push(RenderBuffer::new(size, &self.shm)?);
                self.buffers.len() - 1
            }
        };
        let Some(frame) = self.pending_frame.as_ref() else {
            return Ok(());
        };
        let Some(layer) = self.layer.as_ref() else {
            bail!("the compositor closed the wallpaper surface before presentation");
        };
        let item = &mut self.buffers[index];
        item.buffer
            .canvas(&mut item.pool)
            .context("wallpaper buffer has not been released by the compositor")?
            .copy_from_slice(frame);
        let surface = layer.wl_surface();
        surface.set_buffer_scale(size.scale);
        item.buffer
            .attach_to(surface)
            .context("could not attach the wallpaper buffer")?;
        surface.damage_buffer(0, 0, size.width as i32, size.height as i32);
        if self.video {
            surface.frame(qh, surface.clone());
            self.callback_pending = true;
        }
        layer.commit();
        self.pending_frame = None;
        Ok(())
    }
}

impl OutputHandler for Wallpaper {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.catalog.outputs
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}

    fn output_destroyed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        if self.selected.as_ref() == Some(&output) {
            self.failure = Some(anyhow!(
                "the selected Wayland output was removed; wallpaper stopped"
            ));
            self.layer = None;
        }
    }
}

impl LayerShellHandler for Wallpaper {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.closed = true;
        self.layer = None;
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        self.logical_size = Some(configure.new_size);
    }
}

impl CompositorHandler for Wallpaper {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        new_factor: i32,
    ) {
        self.scale = new_factor;
    }

    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
        // Buffers retain the normal transform; the compositor rotates the output.
    }

    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {
        self.callback_pending = false;
    }

    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl ShmHandler for Wallpaper {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for Wallpaper {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.catalog.registry
    }
    registry_handlers![OutputState];
}

delegate_compositor!(Wallpaper);
delegate_output!(Wallpaper);
delegate_layer!(Wallpaper);
delegate_shm!(Wallpaper);
delegate_registry!(Wallpaper);
wayland_client::delegate_noop!(Wallpaper: ignore wl_region::WlRegion);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_preserves_compositor_order_and_exact_names() {
        let names = [Some("DP-2"), Some("HDMI-A-1"), Some("dp-2")];
        assert_eq!(select_output(&names, None).unwrap(), 0);
        assert_eq!(select_output(&names, Some("HDMI-A-1")).unwrap(), 1);
        assert_eq!(select_output(&names, Some("dp-2")).unwrap(), 2);
        let error = select_output(&names, Some("DP-1")).unwrap_err().to_string();
        assert!(error.contains("DP-1"));
        assert!(error.contains("DP-2, HDMI-A-1, dp-2"));
    }

    #[test]
    fn selection_rejects_absent_or_ambiguous_outputs_without_fallback() {
        assert!(select_output(&[], None).is_err());
        assert!(select_output(&[Some("DP-1"), Some("DP-1")], Some("DP-1")).is_err());
        assert_eq!(select_output(&[None, Some("DP-1")], None).unwrap(), 0);
        assert!(select_output(&[None], Some("")).is_err());
    }

    #[test]
    fn framebuffer_uses_physical_pixels_and_tracks_scale_changes() {
        let size = FrameSize::new((1920, 1080), 2).unwrap();
        assert_eq!((size.width, size.height), (3840, 2160));
        assert_eq!(size.stride, 3840 * 4);
        assert_eq!(size.bytes, 3840 * 2160 * 4);
        assert_eq!(size.scale, 2);
        assert_ne!(size, FrameSize::new((1920, 1080), 1).unwrap());
    }

    #[test]
    fn framebuffer_rejects_invalid_protocol_dimensions_and_arithmetic_overflow() {
        for (logical, scale) in [
            ((0, 1080), 1),
            ((1920, 0), 1),
            ((1920, 1080), 0),
            ((1920, 1080), -1),
            ((u32::MAX, 1), 2),
            ((1, u32::MAX), 2),
            ((i32::MAX as u32 + 1, 1), 1),
            ((1, i32::MAX as u32 + 1), 1),
            ((i32::MAX as u32 / 4 + 1, 1), 1),
        ] {
            assert!(
                FrameSize::new(logical, scale).is_err(),
                "{logical:?} at scale {scale}"
            );
        }
    }
}
