use std::{
    error::Error,
    ffi::{CStr, CString},
    fs::File,
    io::{Read, Seek, SeekFrom},
    mem,
    os::fd::AsFd,
    ptr,
    time::{Duration, Instant},
};

use sdl2::{
    event::Event,
    keyboard::Keycode,
    mouse::{MouseButton, MouseUtil},
    video::{GLContext, GLProfile, Window},
};
use smithay_client_toolkit::{
    delegate_output, delegate_registry,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
};
use tempfile::tempfile;
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, WEnum, delegate_noop,
    globals::registry_queue_init,
    protocol::{wl_buffer, wl_output, wl_shm, wl_shm_pool},
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::{self, ZwlrScreencopyManagerV1},
};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

const MIN_ZOOM_FACTOR: f32 = 0.15;
const MAX_ZOOM_FACTOR: f32 = 24.0;
const KEY_ZOOM_STEP: f32 = 1.18;
const WHEEL_ZOOM_STEP: f32 = 1.14;
const ARROW_PAN_STEP: f32 = 72.0;
const SPOTLIGHT_RADIUS: f32 = 70.0;
const SPOTLIGHT_TINT: f32 = 0.42;
const START_FADE_DURATION: Duration = Duration::from_millis(180);

struct Screenshot {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

struct CaptureState {
    registry_state: RegistryState,
    output_state: OutputState,
    shm: wl_shm::WlShm,
    capture: Capture,
    done: bool,
    error: Option<String>,
}

struct Capture {
    frame: ZwlrScreencopyFrameV1,
    file: Option<File>,
    buffer: Option<wl_buffer::WlBuffer>,
    pool: Option<wl_shm_pool::WlShmPool>,
    width: u32,
    height: u32,
    stride: u32,
    format: wl_shm::Format,
    y_inverted: bool,
    screenshot: Option<Screenshot>,
}

impl Capture {
    fn new(frame: ZwlrScreencopyFrameV1) -> Self {
        Self {
            frame,
            file: None,
            buffer: None,
            pool: None,
            width: 0,
            height: 0,
            stride: 0,
            format: wl_shm::Format::Xrgb8888,
            y_inverted: false,
            screenshot: None,
        }
    }
}

impl CaptureState {
    fn prepare_buffer(
        &mut self,
        frame: &ZwlrScreencopyFrameV1,
        qh: &QueueHandle<Self>,
        format: wl_shm::Format,
        width: u32,
        height: u32,
        stride: u32,
    ) -> Result<()> {
        if self.capture.frame.id() != frame.id() {
            return Err("received buffer event for an unknown frame".into());
        }

        let size = stride
            .checked_mul(height)
            .ok_or("screenshot buffer size overflowed")?;
        let file = tempfile()?;
        file.set_len(size as u64)?;

        let pool = self.shm.create_pool(file.as_fd(), size as i32, qh, ());
        let buffer = pool.create_buffer(
            0,
            width as i32,
            height as i32,
            stride as i32,
            format,
            qh,
            (),
        );

        frame.copy(&buffer);

        self.capture.file = Some(file);
        self.capture.pool = Some(pool);
        self.capture.buffer = Some(buffer);
        self.capture.width = width;
        self.capture.height = height;
        self.capture.stride = stride;
        self.capture.format = format;

        Ok(())
    }

    fn finish_frame(&mut self, frame: &ZwlrScreencopyFrameV1) -> Result<()> {
        if self.capture.frame.id() != frame.id() {
            return Err("received ready event for an unknown frame".into());
        }

        self.capture.screenshot = Some(read_screenshot(&mut self.capture)?);
        self.done = true;
        frame.destroy();

        Ok(())
    }

    fn fail(&mut self, error: impl Into<String>) {
        self.error = Some(error.into());
        self.done = true;
    }
}

impl ProvidesRegistryState for CaptureState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    registry_handlers!(OutputState);
}

impl OutputHandler for CaptureState {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}

    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}

    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl Dispatch<ZwlrScreencopyManagerV1, ()> for CaptureState {
    fn event(
        _: &mut Self,
        _: &ZwlrScreencopyManagerV1,
        _: zwlr_screencopy_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for CaptureState {
    fn event(
        state: &mut Self,
        frame: &ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_screencopy_frame_v1::Event::Buffer {
                format: WEnum::Value(format),
                width,
                height,
                stride,
            } => {
                if let Err(error) = state.prepare_buffer(frame, qh, format, width, height, stride) {
                    state.fail(error.to_string());
                }
            }
            zwlr_screencopy_frame_v1::Event::Buffer {
                format: WEnum::Unknown(format),
                ..
            } => state.fail(format!("unsupported wl_shm format advertised: {format}")),
            zwlr_screencopy_frame_v1::Event::Flags {
                flags: WEnum::Value(flags),
            } => {
                state.capture.y_inverted = flags.contains(zwlr_screencopy_frame_v1::Flags::YInvert);
            }
            zwlr_screencopy_frame_v1::Event::Ready { .. } => {
                if let Err(error) = state.finish_frame(frame) {
                    state.fail(error.to_string());
                }
            }
            zwlr_screencopy_frame_v1::Event::Failed => {
                frame.destroy();
                state.fail("screenshot failed by compositor");
            }
            _ => {}
        }
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("zoomaway: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let screenshot = capture_screenshot()?;
    show_zoom_window(screenshot)
}

fn capture_screenshot() -> Result<Screenshot> {
    let conn = Connection::connect_to_env()?;
    let (globals, mut event_queue) = registry_queue_init(&conn)?;
    let qh = event_queue.handle();

    let shm = globals.bind::<wl_shm::WlShm, _, _>(&qh, 1..=1, ())?;
    let manager = globals.bind::<ZwlrScreencopyManagerV1, _, _>(&qh, 1..=1, ())?;

    let output_state = OutputState::new(&globals, &qh);
    let output = output_state
        .outputs()
        .next()
        .ok_or("no Wayland outputs found")?;
    let frame = manager.capture_output(0, &output, &qh, ());

    let mut state = CaptureState {
        registry_state: RegistryState::new(&globals),
        output_state,
        shm,
        capture: Capture::new(frame),
        done: false,
        error: None,
    };

    while !state.done {
        event_queue.blocking_dispatch(&mut state)?;
    }

    if let Some(error) = state.error {
        Err(error.into())
    } else {
        state
            .capture
            .screenshot
            .ok_or_else(|| "compositor finished without screenshot data".into())
    }
}

fn read_screenshot(capture: &mut Capture) -> Result<Screenshot> {
    let file = capture
        .file
        .as_mut()
        .ok_or("compositor signaled ready before a buffer was created")?;
    let size = capture
        .stride
        .checked_mul(capture.height)
        .ok_or("screenshot buffer size overflowed")? as usize;

    let mut pixels = vec![0; size];
    file.seek(SeekFrom::Start(0))?;
    file.read_exact(&mut pixels)?;

    Ok(Screenshot {
        width: capture.width,
        height: capture.height,
        rgba: convert_to_rgba(
            &pixels,
            capture.width,
            capture.height,
            capture.stride,
            capture.format,
            capture.y_inverted,
        )?,
    })
}

fn convert_to_rgba(
    pixels: &[u8],
    width: u32,
    height: u32,
    stride: u32,
    format: wl_shm::Format,
    y_inverted: bool,
) -> Result<Vec<u8>> {
    let mut rgba = vec![0; (width * height * 4) as usize];

    for y in 0..height {
        let source_y = if y_inverted { height - 1 - y } else { y };
        for x in 0..width {
            let source = (source_y * stride + x * 4) as usize;
            let target = ((y * width + x) * 4) as usize;

            let [r, g, b, a] = match format {
                wl_shm::Format::Argb8888 => [
                    pixels[source + 2],
                    pixels[source + 1],
                    pixels[source],
                    pixels[source + 3],
                ],
                wl_shm::Format::Xrgb8888 => {
                    [pixels[source + 2], pixels[source + 1], pixels[source], 255]
                }
                wl_shm::Format::Abgr8888 => [
                    pixels[source],
                    pixels[source + 1],
                    pixels[source + 2],
                    pixels[source + 3],
                ],
                wl_shm::Format::Xbgr8888 => {
                    [pixels[source], pixels[source + 1], pixels[source + 2], 255]
                }
                wl_shm::Format::Rgba8888 => [
                    pixels[source + 3],
                    pixels[source + 2],
                    pixels[source + 1],
                    pixels[source],
                ],
                wl_shm::Format::Rgbx8888 => [
                    pixels[source + 3],
                    pixels[source + 2],
                    pixels[source + 1],
                    255,
                ],
                wl_shm::Format::Bgra8888 => [
                    pixels[source + 1],
                    pixels[source + 2],
                    pixels[source + 3],
                    pixels[source],
                ],
                wl_shm::Format::Bgrx8888 => [
                    pixels[source + 1],
                    pixels[source + 2],
                    pixels[source + 3],
                    255,
                ],
                unsupported => {
                    return Err(format!("unsupported wl_shm format: {unsupported:?}").into());
                }
            };

            rgba[target..target + 4].copy_from_slice(&[r, g, b, a]);
        }
    }

    Ok(rgba)
}

struct Viewer {
    gl: GlResources,
    window: Window,
    _context: GLContext,
    event_pump: sdl2::EventPump,
    mouse: MouseUtil,
    image_width: f32,
    image_height: f32,
    zoom: f32,
    pan: [f32; 2],
    min_zoom: f32,
    max_zoom: f32,
    cursor: [f32; 2],
    dragging: bool,
    spotlight: bool,
    started_at: Instant,
}

impl Viewer {
    fn new(screenshot: Screenshot) -> Result<Self> {
        let sdl = sdl2::init()?;
        let video = sdl.video()?;
        let gl_attr = video.gl_attr();
        gl_attr.set_context_profile(GLProfile::Core);
        gl_attr.set_context_version(3, 3);
        gl_attr.set_double_buffer(true);

        let display = video.current_display_mode(0)?;
        let window = video
            .window("zoomaway", display.w as u32, display.h as u32)
            .opengl()
            .position_centered()
            .fullscreen_desktop()
            .allow_highdpi()
            .build()
            .map_err(|error| error.to_string())?;
        let context = window.gl_create_context()?;
        window.gl_make_current(&context)?;
        video.gl_set_swap_interval(1)?;

        gl::load_with(|name| video.gl_get_proc_address(name) as *const _);

        let gl = GlResources::new(&screenshot)?;
        let mouse = sdl.mouse();
        let event_pump = sdl.event_pump()?;
        let (width, height) = window.drawable_size();
        let fit_zoom = fit_zoom(
            width as f32,
            height as f32,
            screenshot.width as f32,
            screenshot.height as f32,
        );
        let zoom = fit_zoom.clamp(fit_zoom * MIN_ZOOM_FACTOR, MAX_ZOOM_FACTOR);

        Ok(Self {
            gl,
            window,
            _context: context,
            event_pump,
            mouse,
            image_width: screenshot.width as f32,
            image_height: screenshot.height as f32,
            zoom,
            pan: [0.0, 0.0],
            min_zoom: fit_zoom * MIN_ZOOM_FACTOR,
            max_zoom: MAX_ZOOM_FACTOR,
            cursor: [width as f32 * 0.5, height as f32 * 0.5],
            dragging: false,
            spotlight: false,
            started_at: Instant::now(),
        })
    }

    fn run(&mut self) -> Result<()> {
        'running: loop {
            while let Some(event) = self.event_pump.poll_event() {
                match event {
                    Event::Quit { .. }
                    | Event::KeyDown {
                        keycode: Some(Keycode::Escape | Keycode::Q),
                        ..
                    } => break 'running,
                    Event::KeyDown {
                        keycode: Some(Keycode::W),
                        ..
                    } => self.zoom_at_cursor(KEY_ZOOM_STEP),
                    Event::KeyDown {
                        keycode: Some(Keycode::S),
                        ..
                    } => self.zoom_at_cursor(1.0 / KEY_ZOOM_STEP),
                    Event::KeyDown {
                        keycode: Some(Keycode::F),
                        repeat: false,
                        ..
                    } => self.set_spotlight(!self.spotlight),
                    Event::KeyDown {
                        keycode: Some(Keycode::Left),
                        ..
                    } => self.pan_by(ARROW_PAN_STEP, 0.0),
                    Event::KeyDown {
                        keycode: Some(Keycode::Right),
                        ..
                    } => self.pan_by(-ARROW_PAN_STEP, 0.0),
                    Event::KeyDown {
                        keycode: Some(Keycode::Up),
                        ..
                    } => self.pan_by(0.0, ARROW_PAN_STEP),
                    Event::KeyDown {
                        keycode: Some(Keycode::Down),
                        ..
                    } => self.pan_by(0.0, -ARROW_PAN_STEP),
                    Event::MouseWheel { y, precise_y, .. } => {
                        let amount = if precise_y.abs() > 0.0 {
                            precise_y
                        } else {
                            y as f32
                        };
                        if amount != 0.0 {
                            self.zoom_at_cursor(WHEEL_ZOOM_STEP.powf(amount));
                        }
                    }
                    Event::MouseButtonDown {
                        mouse_btn: MouseButton::Left,
                        ..
                    } => self.dragging = true,
                    Event::MouseButtonUp {
                        mouse_btn: MouseButton::Left,
                        ..
                    } => self.dragging = false,
                    Event::MouseMotion {
                        x, y, xrel, yrel, ..
                    } => {
                        self.cursor = [x as f32, y as f32];
                        if self.dragging {
                            self.pan_by(xrel as f32, yrel as f32);
                        }
                    }
                    _ => {}
                }
            }

            self.render()?;
        }

        Ok(())
    }

    fn zoom_at_cursor(&mut self, factor: f32) {
        let (view_width, view_height) = self.drawable_size();
        let center = [view_width * 0.5, view_height * 0.5];
        let image_x = (self.cursor[0] - center[0] - self.pan[0]) / self.zoom;
        let image_y = (self.cursor[1] - center[1] - self.pan[1]) / self.zoom;
        let next_zoom = (self.zoom * factor).clamp(self.min_zoom, self.max_zoom);

        self.pan[0] = self.cursor[0] - center[0] - image_x * next_zoom;
        self.pan[1] = self.cursor[1] - center[1] - image_y * next_zoom;
        self.zoom = next_zoom;
        self.clamp_pan();
    }

    fn pan_by(&mut self, dx: f32, dy: f32) {
        self.pan[0] += dx;
        self.pan[1] += dy;
        self.clamp_pan();
    }

    fn set_spotlight(&mut self, enabled: bool) {
        self.spotlight = enabled;
        self.mouse.show_cursor(!enabled);
    }

    fn clamp_pan(&mut self) {
        let (view_width, view_height) = self.drawable_size();
        let max_x = (self.image_width * self.zoom + view_width) * 0.5;
        let max_y = (self.image_height * self.zoom + view_height) * 0.5;

        self.pan[0] = self.pan[0].clamp(-max_x, max_x);
        self.pan[1] = self.pan[1].clamp(-max_y, max_y);
    }

    fn drawable_size(&self) -> (f32, f32) {
        let (width, height) = self.window.drawable_size();
        (width.max(1) as f32, height.max(1) as f32)
    }

    fn render(&mut self) -> Result<()> {
        let (view_width, view_height) = self.drawable_size();
        let draw_width = self.image_width * self.zoom;
        let draw_height = self.image_height * self.zoom;
        let left = view_width * 0.5 + self.pan[0] - draw_width * 0.5;
        let top = view_height * 0.5 + self.pan[1] - draw_height * 0.5;
        let right = left + draw_width;
        let bottom = top + draw_height;

        self.gl.draw(
            view_width,
            view_height,
            [left, top, right, bottom],
            self.cursor,
            self.spotlight,
            self.fade_alpha(),
        );
        self.window.gl_swap_window();

        Ok(())
    }

    fn fade_alpha(&self) -> f32 {
        let elapsed = self.started_at.elapsed().as_secs_f32();
        let duration = START_FADE_DURATION.as_secs_f32();

        (elapsed / duration).clamp(0.0, 1.0)
    }
}

impl Drop for Viewer {
    fn drop(&mut self) {
        self.mouse.show_cursor(true);
    }
}

fn show_zoom_window(screenshot: Screenshot) -> Result<()> {
    let mut viewer = Viewer::new(screenshot)?;
    viewer.run()
}

fn fit_zoom(view_width: f32, view_height: f32, image_width: f32, image_height: f32) -> f32 {
    (view_width / image_width)
        .min(view_height / image_height)
        .max(0.01)
}

struct GlResources {
    program: u32,
    vao: u32,
    vbo: u32,
    texture: u32,
    viewport_uniform: i32,
    spotlight_center_uniform: i32,
    spotlight_radius_uniform: i32,
    spotlight_tint_uniform: i32,
    spotlight_enabled_uniform: i32,
    fade_alpha_uniform: i32,
}

impl GlResources {
    fn new(screenshot: &Screenshot) -> Result<Self> {
        let program = create_program()?;
        let mut vao = 0;
        let mut vbo = 0;
        let mut texture = 0;

        unsafe {
            gl::GenVertexArrays(1, &mut vao);
            gl::GenBuffers(1, &mut vbo);
            gl::BindVertexArray(vao);
            gl::BindBuffer(gl::ARRAY_BUFFER, vbo);
            gl::BufferData(
                gl::ARRAY_BUFFER,
                (mem::size_of::<f32>() * 24) as isize,
                ptr::null(),
                gl::STREAM_DRAW,
            );

            let stride = (4 * mem::size_of::<f32>()) as i32;
            gl::EnableVertexAttribArray(0);
            gl::VertexAttribPointer(0, 2, gl::FLOAT, gl::FALSE, stride, ptr::null());
            gl::EnableVertexAttribArray(1);
            gl::VertexAttribPointer(
                1,
                2,
                gl::FLOAT,
                gl::FALSE,
                stride,
                (2 * mem::size_of::<f32>()) as *const _,
            );

            gl::GenTextures(1, &mut texture);
            gl::BindTexture(gl::TEXTURE_2D, texture);
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::LINEAR as i32);
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, gl::LINEAR as i32);
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_S, gl::CLAMP_TO_EDGE as i32);
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_T, gl::CLAMP_TO_EDGE as i32);
            gl::PixelStorei(gl::UNPACK_ALIGNMENT, 1);
            gl::TexImage2D(
                gl::TEXTURE_2D,
                0,
                gl::RGBA as i32,
                screenshot.width as i32,
                screenshot.height as i32,
                0,
                gl::RGBA,
                gl::UNSIGNED_BYTE,
                screenshot.rgba.as_ptr() as *const _,
            );
        }

        let viewport_uniform =
            unsafe { gl::GetUniformLocation(program, CString::new("viewport")?.as_ptr()) };
        let spotlight_center_uniform =
            unsafe { gl::GetUniformLocation(program, CString::new("spotlight_center")?.as_ptr()) };
        let spotlight_radius_uniform =
            unsafe { gl::GetUniformLocation(program, CString::new("spotlight_radius")?.as_ptr()) };
        let spotlight_tint_uniform =
            unsafe { gl::GetUniformLocation(program, CString::new("spotlight_tint")?.as_ptr()) };
        let spotlight_enabled_uniform =
            unsafe { gl::GetUniformLocation(program, CString::new("spotlight_enabled")?.as_ptr()) };
        let fade_alpha_uniform =
            unsafe { gl::GetUniformLocation(program, CString::new("fade_alpha")?.as_ptr()) };

        Ok(Self {
            program,
            vao,
            vbo,
            texture,
            viewport_uniform,
            spotlight_center_uniform,
            spotlight_radius_uniform,
            spotlight_tint_uniform,
            spotlight_enabled_uniform,
            fade_alpha_uniform,
        })
    }

    fn draw(
        &self,
        view_width: f32,
        view_height: f32,
        rect: [f32; 4],
        cursor: [f32; 2],
        spotlight: bool,
        fade_alpha: f32,
    ) {
        let [left, top, right, bottom] = rect;
        let vertices: [f32; 24] = [
            left, top, 0.0, 0.0, right, top, 1.0, 0.0, right, bottom, 1.0, 1.0, left, top, 0.0,
            0.0, right, bottom, 1.0, 1.0, left, bottom, 0.0, 1.0,
        ];

        unsafe {
            gl::Viewport(0, 0, view_width as i32, view_height as i32);
            gl::ClearColor(0.02, 0.02, 0.02, 1.0);
            gl::Clear(gl::COLOR_BUFFER_BIT);
            gl::UseProgram(self.program);
            gl::Uniform2f(self.viewport_uniform, view_width, view_height);
            gl::Uniform2f(
                self.spotlight_center_uniform,
                cursor[0],
                view_height - cursor[1],
            );
            gl::Uniform1f(self.spotlight_radius_uniform, SPOTLIGHT_RADIUS);
            gl::Uniform1f(self.spotlight_tint_uniform, SPOTLIGHT_TINT);
            gl::Uniform1i(self.spotlight_enabled_uniform, i32::from(spotlight));
            gl::Uniform1f(self.fade_alpha_uniform, fade_alpha);
            gl::ActiveTexture(gl::TEXTURE0);
            gl::BindTexture(gl::TEXTURE_2D, self.texture);
            gl::BindVertexArray(self.vao);
            gl::BindBuffer(gl::ARRAY_BUFFER, self.vbo);
            gl::BufferSubData(
                gl::ARRAY_BUFFER,
                0,
                (vertices.len() * mem::size_of::<f32>()) as isize,
                vertices.as_ptr() as *const _,
            );
            gl::DrawArrays(gl::TRIANGLES, 0, 6);
        }
    }
}

impl Drop for GlResources {
    fn drop(&mut self) {
        unsafe {
            gl::DeleteTextures(1, &self.texture);
            gl::DeleteBuffers(1, &self.vbo);
            gl::DeleteVertexArrays(1, &self.vao);
            gl::DeleteProgram(self.program);
        }
    }
}

fn create_program() -> Result<u32> {
    let vertex_shader = compile_shader(gl::VERTEX_SHADER, VERTEX_SHADER)?;
    let fragment_shader = compile_shader(gl::FRAGMENT_SHADER, FRAGMENT_SHADER)?;
    let program = unsafe { gl::CreateProgram() };

    unsafe {
        gl::AttachShader(program, vertex_shader);
        gl::AttachShader(program, fragment_shader);
        gl::LinkProgram(program);
        gl::DeleteShader(vertex_shader);
        gl::DeleteShader(fragment_shader);
    }

    let mut success = 0;
    unsafe {
        gl::GetProgramiv(program, gl::LINK_STATUS, &mut success);
    }

    if success == 0 {
        let log = program_log(program);
        unsafe {
            gl::DeleteProgram(program);
        }
        Err(format!("OpenGL program link failed: {log}").into())
    } else {
        Ok(program)
    }
}

fn compile_shader(kind: u32, source: &str) -> Result<u32> {
    let shader = unsafe { gl::CreateShader(kind) };
    let source = CString::new(source)?;

    unsafe {
        gl::ShaderSource(shader, 1, &source.as_ptr(), ptr::null());
        gl::CompileShader(shader);
    }

    let mut success = 0;
    unsafe {
        gl::GetShaderiv(shader, gl::COMPILE_STATUS, &mut success);
    }

    if success == 0 {
        let log = shader_log(shader);
        unsafe {
            gl::DeleteShader(shader);
        }
        Err(format!("OpenGL shader compile failed: {log}").into())
    } else {
        Ok(shader)
    }
}

fn shader_log(shader: u32) -> String {
    let mut length = 0;
    unsafe {
        gl::GetShaderiv(shader, gl::INFO_LOG_LENGTH, &mut length);
    }
    let mut buffer = vec![0; length.max(1) as usize];
    unsafe {
        gl::GetShaderInfoLog(
            shader,
            length,
            ptr::null_mut(),
            buffer.as_mut_ptr() as *mut _,
        );
    }
    c_log_to_string(&buffer)
}

fn program_log(program: u32) -> String {
    let mut length = 0;
    unsafe {
        gl::GetProgramiv(program, gl::INFO_LOG_LENGTH, &mut length);
    }
    let mut buffer = vec![0; length.max(1) as usize];
    unsafe {
        gl::GetProgramInfoLog(
            program,
            length,
            ptr::null_mut(),
            buffer.as_mut_ptr() as *mut _,
        );
    }
    c_log_to_string(&buffer)
}

fn c_log_to_string(buffer: &[u8]) -> String {
    CStr::from_bytes_until_nul(buffer)
        .map(|message| message.to_string_lossy().into_owned())
        .unwrap_or_default()
}

const VERTEX_SHADER: &str = r#"#version 330 core
layout (location = 0) in vec2 position;
layout (location = 1) in vec2 tex_coord;

uniform vec2 viewport;
out vec2 uv;

void main() {
    vec2 ndc = vec2(
        (position.x / viewport.x) * 2.0 - 1.0,
        1.0 - (position.y / viewport.y) * 2.0
    );
    gl_Position = vec4(ndc, 0.0, 1.0);
    uv = tex_coord;
}
"#;

const FRAGMENT_SHADER: &str = r#"#version 330 core
in vec2 uv;

uniform sampler2D image_texture;
uniform vec2 spotlight_center;
uniform float spotlight_radius;
uniform float spotlight_tint;
uniform int spotlight_enabled;
uniform float fade_alpha;
out vec4 color;

void main() {
    vec4 pixel = texture(image_texture, uv);

    if (spotlight_enabled == 1) {
        float distance_from_center = distance(gl_FragCoord.xy, spotlight_center);
        if (distance_from_center > spotlight_radius) {
            pixel.rgb *= spotlight_tint;
        }
    }

    color = vec4(pixel.rgb * fade_alpha, pixel.a * fade_alpha);
}
"#;

delegate_registry!(CaptureState);
delegate_output!(CaptureState);
delegate_noop!(CaptureState: ignore wl_shm::WlShm);
delegate_noop!(CaptureState: ignore wl_shm_pool::WlShmPool);
delegate_noop!(CaptureState: ignore wl_buffer::WlBuffer);
