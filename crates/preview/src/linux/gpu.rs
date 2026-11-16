//! EGL + GLES2 on the Wayland display, and GBM buffer allocation on the same GPU.
//!
//! Captured windows arrive as dmabufs the compositor rendered into; they're imported as EGLImages bound to GL
//! textures and sampled straight into each thumbnail surface, so window pixels never touch the CPU.

use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;

use eve_maj_core::log::Scope;
use glow::HasContext;
use khronos_egl as egl;

const SLOG: Scope = Scope::new("gpu");

type Egl = egl::Instance<egl::Static>;

const EGL_PLATFORM_WAYLAND_KHR: egl::Enum = 0x31D8;
const EGL_LINUX_DMA_BUF_EXT: egl::Enum = 0x3270;
const EGL_LINUX_DRM_FOURCC_EXT: usize = 0x3271;
const EGL_DEVICE_EXT: egl::Int = 0x322C;
const EGL_DRM_DEVICE_FILE_EXT: egl::Int = 0x3233;
const EGL_DRM_RENDER_NODE_FILE_EXT: egl::Int = 0x3377;
const PLANE_FD: [usize; 4] = [0x3272, 0x3275, 0x3278, 0x3440];
const PLANE_OFFSET: [usize; 4] = [0x3273, 0x3276, 0x3279, 0x3441];
const PLANE_PITCH: [usize; 4] = [0x3274, 0x3277, 0x327A, 0x3442];
const PLANE_MOD_LO: [usize; 4] = [0x3443, 0x3445, 0x3447, 0x3449];
const PLANE_MOD_HI: [usize; 4] = [0x3444, 0x3446, 0x3448, 0x344A];
const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

type ImageTargetTexture2d = unsafe extern "system" fn(target: u32, image: *const c_void);
type QueryDisplayAttrib = unsafe extern "system" fn(dpy: *mut c_void, attribute: egl::Int, value: *mut isize) -> egl::Boolean;
type QueryDeviceString = unsafe extern "system" fn(device: *mut c_void, name: egl::Int) -> *const std::ffi::c_char;

const VERTEX_SHADER: &str = "attribute vec2 pos;
varying vec2 uv;
uniform float yflip;
void main() {
  uv = vec2(pos.x, yflip > 0.5 ? pos.y : 1.0 - pos.y);
  gl_Position = vec4(pos * 2.0 - 1.0, 0.0, 1.0);
}";

// Output is premultiplied: the window image is opaque content scaled by `alpha`; the overlay is already premultiplied.
const FRAGMENT_SHADER: &str = "precision mediump float;
varying vec2 uv;
uniform sampler2D tex;
uniform float swap_rb;
uniform float use_alpha;
uniform float alpha;
void main() {
  vec4 c = texture2D(tex, uv);
  if (swap_rb > 0.5) c = c.bgra;
  if (use_alpha < 0.5) c.a = 1.0;
  gl_FragColor = c * alpha;
}";

/// One plane of a dmabuf to import.
pub struct DmabufPlane {
    pub fd: i32,
    pub offset: u32,
    pub stride: u32,
}

pub struct Gpu {
    pub egl: Egl,
    pub display: egl::Display,
    config: egl::Config,
    pub context: egl::Context,
    pub gl: glow::Context,
    image_target: Option<ImageTargetTexture2d>,
    program: glow::Program,
    vbo: glow::Buffer,
    u_tex: Option<glow::UniformLocation>,
    u_swap: Option<glow::UniformLocation>,
    u_yflip: Option<glow::UniformLocation>,
    u_use_alpha: Option<glow::UniformLocation>,
    u_alpha: Option<glow::UniformLocation>,
    pub gbm: Option<gbm::Device<File>>,
    pub renderer: String,
}

impl Gpu {
    /// # Safety
    /// `wl_display` must be the live `wl_display*` of the connection the surfaces will belong to.
    pub unsafe fn new(wl_display: *mut c_void) -> Result<Gpu, String> {
        let egl = egl::Instance::new(egl::Static);
        let display = egl
            .get_platform_display(EGL_PLATFORM_WAYLAND_KHR, wl_display, &[egl::ATTRIB_NONE])
            .map_err(|e| format!("eglGetPlatformDisplay: {e}"))?;
        egl.initialize(display).map_err(|e| format!("eglInitialize: {e}"))?;
        egl.bind_api(egl::OPENGL_ES_API).map_err(|e| format!("eglBindAPI: {e}"))?;
        let attrs = [
            egl::SURFACE_TYPE, egl::WINDOW_BIT,
            egl::RENDERABLE_TYPE, egl::OPENGL_ES2_BIT,
            egl::RED_SIZE, 8, egl::GREEN_SIZE, 8, egl::BLUE_SIZE, 8, egl::ALPHA_SIZE, 8,
            egl::NONE,
        ];
        let config = egl.choose_first_config(display, &attrs).map_err(|e| format!("eglChooseConfig: {e}"))?.ok_or("no RGBA8 GLES2 EGL config")?;
        let context = egl
            .create_context(display, config, None, &[egl::CONTEXT_CLIENT_VERSION, 2, egl::NONE])
            .map_err(|e| format!("eglCreateContext: {e}"))?;
        egl.make_current(display, None, None, Some(context)).map_err(|e| format!("surfaceless eglMakeCurrent (EGL_KHR_surfaceless_context): {e}"))?;

        let gl = glow::Context::from_loader_function(|s| egl.get_proc_address(s).map_or(std::ptr::null(), |f| f as *const c_void));
        let renderer = format!("{} / {}", gl.get_parameter_string(glow::VENDOR), gl.get_parameter_string(glow::RENDERER));

        let egl_exts = egl.query_string(Some(display), egl::EXTENSIONS).map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let gl_exts = gl.supported_extensions();
        let image_target: Option<ImageTargetTexture2d> = (egl_exts.split(' ').any(|e| e == "EGL_EXT_image_dma_buf_import") && gl_exts.contains("GL_OES_EGL_image"))
            .then(|| egl.get_proc_address("glEGLImageTargetTexture2DOES"))
            .flatten()
            .map(|f| std::mem::transmute::<extern "system" fn(), ImageTargetTexture2d>(f));

        let gbm = if image_target.is_some() { open_gbm(&egl, display) } else { None };

        let program = gl.create_program()?;
        for (kind, src) in [(glow::VERTEX_SHADER, VERTEX_SHADER), (glow::FRAGMENT_SHADER, FRAGMENT_SHADER)] {
            let shader = gl.create_shader(kind)?;
            gl.shader_source(shader, src);
            gl.compile_shader(shader);
            if !gl.get_shader_compile_status(shader) {
                return Err(format!("shader: {}", gl.get_shader_info_log(shader)));
            }
            gl.attach_shader(program, shader);
        }
        gl.bind_attrib_location(program, 0, "pos");
        gl.link_program(program);
        if !gl.get_program_link_status(program) {
            return Err(format!("link: {}", gl.get_program_info_log(program)));
        }
        let quad: [f32; 8] = [0.0, 0.0, 1.0, 0.0, 0.0, 1.0, 1.0, 1.0];
        let vbo = gl.create_buffer()?;
        gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
        gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, std::slice::from_raw_parts(quad.as_ptr() as *const u8, 32), glow::STATIC_DRAW);

        Ok(Gpu {
            u_tex: gl.get_uniform_location(program, "tex"),
            u_swap: gl.get_uniform_location(program, "swap_rb"),
            u_yflip: gl.get_uniform_location(program, "yflip"),
            u_use_alpha: gl.get_uniform_location(program, "use_alpha"),
            u_alpha: gl.get_uniform_location(program, "alpha"),
            egl,
            display,
            config,
            context,
            gl,
            image_target,
            program,
            vbo,
            gbm,
            renderer,
        })
    }

    pub fn dmabuf_capable(&self) -> bool {
        self.gbm.is_some() && self.image_target.is_some()
    }

    pub fn make_current_surfaceless(&self) {
        let _ = self.egl.make_current(self.display, None, None, Some(self.context));
    }

    /// # Safety
    /// `native_window` must be a live `wl_egl_window*`.
    pub unsafe fn create_window_surface(&self, native_window: *mut c_void) -> Option<egl::Surface> {
        match self.egl.create_platform_window_surface(self.display, self.config, native_window, &[egl::ATTRIB_NONE]) {
            Ok(s) => Some(s),
            Err(e) => {
                SLOG.err(format_args!("eglCreatePlatformWindowSurface failed: {e}"));
                None
            }
        }
    }

    pub fn destroy_surface(&self, surface: egl::Surface) {
        self.make_current_surfaceless();
        let _ = self.egl.destroy_surface(self.display, surface);
    }

    pub fn new_texture(&self) -> Option<glow::Texture> {
        unsafe {
            let tex = self.gl.create_texture().ok()?;
            self.gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            self.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
            self.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
            self.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
            self.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);
            Some(tex)
        }
    }

    pub fn delete_texture(&self, tex: glow::Texture) {
        unsafe { self.gl.delete_texture(tex) };
    }

    /// Imports a dmabuf as an EGLImage bound to a fresh texture. The plane fds stay owned by the caller.
    pub fn import_dmabuf(&self, width: u32, height: u32, fourcc: u32, modifier: u64, planes: &[DmabufPlane]) -> Option<(egl::Image, glow::Texture)> {
        let image_target = self.image_target?;
        let mut attrs: Vec<usize> = vec![egl::WIDTH as usize, width as usize, egl::HEIGHT as usize, height as usize, EGL_LINUX_DRM_FOURCC_EXT, fourcc as usize];
        for (i, p) in planes.iter().enumerate().take(4) {
            attrs.extend_from_slice(&[PLANE_FD[i], p.fd as usize, PLANE_OFFSET[i], p.offset as usize, PLANE_PITCH[i], p.stride as usize]);
            if modifier != DRM_FORMAT_MOD_INVALID {
                attrs.extend_from_slice(&[PLANE_MOD_LO[i], (modifier & 0xffff_ffff) as usize, PLANE_MOD_HI[i], (modifier >> 32) as usize]);
            }
        }
        attrs.push(egl::ATTRIB_NONE);
        self.make_current_surfaceless();
        let image = unsafe {
            let no_ctx = egl::Context::from_ptr(egl::NO_CONTEXT);
            self.egl.create_image(self.display, no_ctx, EGL_LINUX_DMA_BUF_EXT, egl::ClientBuffer::from_ptr(std::ptr::null_mut()), &attrs)
        };
        let image = match image {
            Ok(i) => i,
            Err(e) => {
                SLOG.err(format_args!("eglCreateImage(dmabuf {width}x{height} mod 0x{modifier:x}) failed: {e}"));
                return None;
            }
        };
        let tex = self.new_texture()?;
        unsafe {
            image_target(glow::TEXTURE_2D, image.as_ptr());
            if self.gl.get_error() != glow::NO_ERROR {
                SLOG.err(format_args!("glEGLImageTargetTexture2DOES failed"));
                self.gl.delete_texture(tex);
                let _ = self.egl.destroy_image(self.display, image);
                return None;
            }
        }
        Some((image, tex))
    }

    pub fn destroy_image(&self, image: egl::Image) {
        let _ = self.egl.destroy_image(self.display, image);
    }

    /// Uploads tightly-packed or strided 4-byte-per-pixel rows into `tex` (bytes stay in memory order; the shader swizzles).
    pub fn upload_rgba(&self, tex: glow::Texture, width: u32, height: u32, stride: u32, data: &[u8]) {
        unsafe {
            self.gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            self.gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 4);
            if stride == width * 4 {
                self.gl.tex_image_2d(glow::TEXTURE_2D, 0, glow::RGBA as i32, width as i32, height as i32, 0, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(Some(&data[..(stride * height) as usize])));
            } else {
                self.gl.tex_image_2d(glow::TEXTURE_2D, 0, glow::RGBA as i32, width as i32, height as i32, 0, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(None));
                for row in 0..height {
                    let start = (row * stride) as usize;
                    self.gl.tex_sub_image_2d(glow::TEXTURE_2D, 0, 0, row as i32, width as i32, 1, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(Some(&data[start..start + (width * 4) as usize])));
                }
            }
        }
    }

    pub fn begin_frame(&self, surface: egl::Surface, width: i32, height: i32) -> bool {
        if self.egl.make_current(self.display, Some(surface), Some(surface), Some(self.context)).is_err() {
            return false;
        }
        unsafe {
            self.gl.viewport(0, 0, width, height);
            self.gl.disable(glow::SCISSOR_TEST);
            self.gl.clear_color(0.0, 0.0, 0.0, 0.0);
            self.gl.clear(glow::COLOR_BUFFER_BIT);
        }
        true
    }

    /// Fills the whole viewport with a premultiplied color.
    pub fn clear(&self, r: f32, g: f32, b: f32, a: f32) {
        unsafe {
            self.gl.clear_color(r * a, g * a, b * a, a);
            self.gl.clear(glow::COLOR_BUFFER_BIT);
        }
    }

    /// Draws `tex` over the whole viewport. `swap_rb` for byte-order BGRA sources (shm captures, cairo); `use_alpha` for already-premultiplied sources.
    pub fn draw(&self, tex: glow::Texture, swap_rb: bool, y_flip: bool, use_alpha: bool, alpha: f32) {
        let gl = &self.gl;
        unsafe {
            gl.use_program(Some(self.program));
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            gl.uniform_1_i32(self.u_tex.as_ref(), 0);
            gl.uniform_1_f32(self.u_swap.as_ref(), swap_rb as u8 as f32);
            gl.uniform_1_f32(self.u_yflip.as_ref(), y_flip as u8 as f32);
            gl.uniform_1_f32(self.u_use_alpha.as_ref(), use_alpha as u8 as f32);
            gl.uniform_1_f32(self.u_alpha.as_ref(), alpha);
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(self.vbo));
            gl.enable_vertex_attrib_array(0);
            gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, 0, 0);
            gl.enable(glow::BLEND);
            gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);
            gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            gl.disable(glow::BLEND);
        }
    }

    pub fn end_frame(&self, surface: egl::Surface) {
        if let Err(e) = self.egl.swap_buffers(self.display, surface) {
            SLOG.warn(format_args!("eglSwapBuffers failed: {e}"));
        }
    }

    /// Never block a swap on vsync; pacing comes from Wayland frame callbacks.
    pub fn set_swap_interval_zero(&self, surface: egl::Surface) {
        if self.egl.make_current(self.display, Some(surface), Some(surface), Some(self.context)).is_ok() {
            let _ = self.egl.swap_interval(self.display, 0);
        }
    }
}

/// GBM on the DRM node backing the EGL display, so buffers we allocate are importable by both EGL and the compositor.
fn open_gbm(egl: &Egl, display: egl::Display) -> Option<gbm::Device<File>> {
    let mut node = std::env::var("EVE_MAJ_DRM_NODE").ok();
    if node.is_none() {
        unsafe {
            let qda = egl.get_proc_address("eglQueryDisplayAttribEXT").map(|f| std::mem::transmute::<extern "system" fn(), QueryDisplayAttrib>(f));
            let qds = egl.get_proc_address("eglQueryDeviceStringEXT").map(|f| std::mem::transmute::<extern "system" fn(), QueryDeviceString>(f));
            if let (Some(qda), Some(qds)) = (qda, qds) {
                let mut device: isize = 0;
                if qda(display.as_ptr(), EGL_DEVICE_EXT, &mut device) != 0 && device != 0 {
                    for name in [EGL_DRM_RENDER_NODE_FILE_EXT, EGL_DRM_DEVICE_FILE_EXT] {
                        let s = qds(device as *mut c_void, name);
                        if !s.is_null() {
                            node = Some(std::ffi::CStr::from_ptr(s).to_string_lossy().into_owned());
                            break;
                        }
                    }
                }
            }
        }
    }
    let node = node.unwrap_or_else(|| "/dev/dri/renderD128".to_owned());
    let file = match OpenOptions::new().read(true).write(true).open(&node) {
        Ok(f) => f,
        Err(e) => {
            SLOG.warn(format_args!("Can't open {node}: {e}"));
            return None;
        }
    };
    let fd = file.as_raw_fd();
    match gbm::Device::new(file) {
        Ok(dev) => {
            SLOG.info(format_args!("GBM device {node} (fd {fd})"));
            Some(dev)
        }
        Err(e) => {
            SLOG.warn(format_args!("gbm_create_device({node}) failed: {e}"));
            None
        }
    }
}
