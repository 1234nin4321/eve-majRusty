//! The settings window's "Identify Displays" on Linux: a short-lived `eve-maj-preview --identify-displays NAME=N ...`
//! process that puts a numbered square near the top-left corner of each named monitor for three seconds, like the
//! Windows build's identify windows (200x200, 48px in, accent frame and number on the dialog's dark background).
//!
//! It's a separate process with its own small Wayland client, so it works whether or not the preview app is running,
//! and it never takes focus or input.

use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::time::{Duration, Instant};

use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{
    wl_buffer::{self, WlBuffer}, wl_compositor::WlCompositor, wl_output::{self, WlOutput}, wl_region::WlRegion, wl_registry::WlRegistry,
    wl_shm::{Format as ShmFormat, WlShm}, wl_shm_pool::WlShmPool, wl_surface::WlSurface,
};
use wayland_client::{delegate_noop, Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1::{self, ZwlrLayerShellV1}, zwlr_layer_surface_v1::{self, ZwlrLayerSurfaceV1}};

use eve_maj_core::types::FontWeight;

use super::overlay::font_description;

pub const FLAG: &str = "--identify-displays";
const NAMESPACE: &str = "eve-maj-identify";
const SIZE: i32 = 200;
const MARGIN: i32 = 48;
const FRAME: f64 = 4.0;
const DURATION: Duration = Duration::from_millis(3000);
/// The dialog's accent (#d9a441) and background (#0b0c0d).
const ACCENT: (f64, f64, f64) = (0xd9 as f64 / 255.0, 0xa4 as f64 / 255.0, 0x41 as f64 / 255.0);
const BACKGROUND: (f64, f64, f64) = (0x0b as f64 / 255.0, 0x0c as f64 / 255.0, 0x0d as f64 / 255.0);
const FONT: &str = "Sans";
const FONT_PX: i32 = 150;

/// `NAME=N` arguments: which connector shows which number (the numbers the settings window lists).
pub fn parse_targets(args: &[String]) -> Vec<(String, u32)> {
    args.iter()
        .filter_map(|a| {
            let (name, n) = a.rsplit_once('=')?;
            Some((name.to_owned(), n.parse().ok().filter(|&n| n > 0)?)).filter(|(name, _)| !name.is_empty())
        })
        .collect()
}

struct Output {
    wl: WlOutput,
    name: String,
    scale: i32,
}

struct Square {
    number: u32,
    scale: i32,
    surface: WlSurface,
    layer: ZwlrLayerSurfaceV1,
    configured: bool,
}

struct State {
    outputs: Vec<Output>,
    squares: Vec<Square>,
    shm: WlShm,
    /// A square whose surface the compositor closed (its monitor went away) is dropped.
    closed: Vec<usize>,
}

/// Shows the squares, waits out the duration, and returns.
pub fn run(args: &[String]) -> Result<(), String> {
    let targets = parse_targets(args);
    if targets.is_empty() {
        return Err(format!("{FLAG} needs NAME=NUMBER arguments"));
    }
    let conn = Connection::connect_to_env().map_err(|e| format!("can't connect to the Wayland display: {e}"))?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn).map_err(|e| format!("registry: {e}"))?;
    let qh = queue.handle();
    let compositor: WlCompositor = globals.bind(&qh, 4..=6, ()).map_err(|_| "compositor lacks wl_compositor")?;
    let layer_shell: ZwlrLayerShellV1 = globals.bind(&qh, 1..=4, ()).map_err(|_| "compositor lacks wlr-layer-shell")?;
    let shm: WlShm = globals.bind(&qh, 1..=1, ()).map_err(|_| "compositor lacks wl_shm")?;
    let outputs = globals.contents().with_list(|list| {
        list.iter()
            .filter(|g| g.interface == WlOutput::interface().name && g.version >= 4)
            .map(|g| Output { wl: globals.registry().bind(g.name, 4, &qh, ()), name: String::new(), scale: 1 })
            .collect()
    });
    let mut state = State { outputs, squares: Vec::new(), shm, closed: Vec::new() };
    // The outputs' names and scales arrive after binding.
    queue.roundtrip(&mut state).map_err(|e| format!("roundtrip: {e}"))?;

    for (name, number) in &targets {
        let Some(output) = state.outputs.iter().find(|o| &o.name == name) else { continue };
        let surface = compositor.create_surface(&qh, ());
        // Clicks go through to whatever is underneath.
        let region: WlRegion = compositor.create_region(&qh, ());
        surface.set_input_region(Some(&region));
        region.destroy();
        let index = state.squares.len();
        let layer = layer_shell.get_layer_surface(&surface, Some(&output.wl), zwlr_layer_shell_v1::Layer::Overlay, NAMESPACE.into(), &qh, index);
        layer.set_size(SIZE as u32, SIZE as u32);
        layer.set_anchor(zwlr_layer_surface_v1::Anchor::Top | zwlr_layer_surface_v1::Anchor::Left);
        layer.set_margin(MARGIN, 0, 0, MARGIN);
        // Measured from the monitor's edge, not from inside bars' reserved space.
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::None);
        surface.commit();
        state.squares.push(Square { number: *number, scale: output.scale.max(1), surface, layer, configured: false });
    }
    if state.squares.is_empty() {
        return Err("none of the named monitors are connected".into());
    }
    conn.flush().map_err(|e| format!("flush: {e}"))?;

    let deadline = Instant::now() + DURATION;
    while Instant::now() < deadline && state.closed.len() < state.squares.len() {
        queue.dispatch_pending(&mut state).map_err(|e| format!("dispatch: {e}"))?;
        conn.flush().map_err(|e| format!("flush: {e}"))?;
        let Some(guard) = conn.prepare_read() else { continue };
        let wait = deadline.saturating_duration_since(Instant::now()).as_millis() as i32;
        let mut pfd = libc::pollfd { fd: guard.connection_fd().as_raw_fd(), events: libc::POLLIN, revents: 0 };
        if unsafe { libc::poll(&mut pfd, 1, wait.max(0)) } > 0 {
            guard.read().map_err(|e| format!("read: {e}"))?;
        }
    }
    for s in &state.squares {
        s.layer.destroy();
        s.surface.destroy();
    }
    let _ = conn.flush();
    Ok(())
}

fn paint(square: &Square, shm: &WlShm, qh: &QueueHandle<State>) {
    let scale = square.scale;
    let px = SIZE * scale;
    let stride = px * 4;
    let len = (stride * px) as usize;
    unsafe {
        let fd = libc::memfd_create(c"eve-maj-identify".as_ptr(), libc::MFD_CLOEXEC);
        if fd < 0 {
            return;
        }
        let fd = OwnedFd::from_raw_fd(fd);
        if libc::ftruncate(fd.as_raw_fd(), len as i64) < 0 {
            return;
        }
        let ptr = libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd.as_raw_fd(), 0);
        if ptr == libc::MAP_FAILED {
            return;
        }
        if let Ok(surface) = cairo::ImageSurface::create_for_data_unsafe(ptr as *mut u8, cairo::Format::ARgb32, px, px, stride) {
            if let Ok(cr) = cairo::Context::new(&surface) {
                draw(&cr, square.number, scale as f64);
            }
            surface.flush();
        }
        let pool: WlShmPool = shm.create_pool(fd.as_fd(), len as i32, qh, ());
        let buffer = pool.create_buffer(0, px, px, stride, ShmFormat::Argb8888, qh, (ptr as usize, len));
        pool.destroy();
        square.surface.set_buffer_scale(scale);
        square.surface.attach(Some(&buffer), 0, 0);
        square.surface.damage_buffer(0, 0, px, px);
        square.surface.commit();
    }
}

fn draw(cr: &cairo::Context, number: u32, scale: f64) {
    cr.scale(scale, scale);
    let size = SIZE as f64;
    cr.set_source_rgb(BACKGROUND.0, BACKGROUND.1, BACKGROUND.2);
    let _ = cr.paint();
    cr.set_source_rgb(ACCENT.0, ACCENT.1, ACCENT.2);
    cr.rectangle(FRAME / 2.0, FRAME / 2.0, size - FRAME, size - FRAME);
    cr.set_line_width(FRAME);
    let _ = cr.stroke();
    let layout = pangocairo::functions::create_layout(cr);
    layout.set_font_description(Some(&font_description(FONT, FONT_PX, FontWeight::Bold)));
    layout.set_text(&number.to_string());
    // Centred on the ink, not the line box, so the digit sits in the middle like DT_VCENTER's.
    let (ink, _) = layout.pixel_extents();
    cr.move_to((size - ink.width() as f64) / 2.0 - ink.x() as f64, (size - ink.height() as f64) / 2.0 - ink.y() as f64);
    pangocairo::functions::show_layout(cr, &layout);
}

impl Dispatch<ZwlrLayerSurfaceV1, usize> for State {
    fn event(state: &mut Self, layer: &ZwlrLayerSurfaceV1, event: zwlr_layer_surface_v1::Event, index: &usize, _: &Connection, qh: &QueueHandle<Self>) {
        match event {
            zwlr_layer_surface_v1::Event::Configure { serial, .. } => {
                layer.ack_configure(serial);
                let Some(square) = state.squares.get_mut(*index) else { return };
                if !square.configured {
                    square.configured = true;
                    paint(&state.squares[*index], &state.shm, qh);
                } else {
                    state.squares[*index].surface.commit();
                }
            }
            zwlr_layer_surface_v1::Event::Closed if !state.closed.contains(index) => state.closed.push(*index),
            _ => {}
        }
    }
}

impl Dispatch<WlBuffer, (usize, usize)> for State {
    fn event(_: &mut Self, buffer: &WlBuffer, event: wl_buffer::Event, mapping: &(usize, usize), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_buffer::Event::Release = event {
            buffer.destroy();
            unsafe { libc::munmap(mapping.0 as *mut libc::c_void, mapping.1) };
        }
    }
}

impl Dispatch<WlOutput, ()> for State {
    fn event(state: &mut Self, output: &WlOutput, event: wl_output::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        let Some(o) = state.outputs.iter_mut().find(|o| &o.wl == output) else { return };
        match event {
            wl_output::Event::Name { name } => o.name = name,
            wl_output::Event::Scale { factor } => o.scale = factor,
            _ => {}
        }
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(_: &mut Self, _: &WlRegistry, _: <WlRegistry as Proxy>::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}

delegate_noop!(State: ignore WlCompositor);
delegate_noop!(State: ignore WlSurface);
delegate_noop!(State: ignore WlRegion);
delegate_noop!(State: ignore WlShm);
delegate_noop!(State: ignore WlShmPool);
delegate_noop!(State: ignore ZwlrLayerShellV1);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_name_number_pairs() {
        let args: Vec<String> = ["DP-1=1", "HDMI-A-1=2", "junk", "=3", "DP-2=0", "DP-3=x"].iter().map(|s| s.to_string()).collect();
        assert_eq!(parse_targets(&args), vec![("DP-1".to_owned(), 1), ("HDMI-A-1".to_owned(), 2)]);
    }
}
