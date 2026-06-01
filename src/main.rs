use std::{
    env,
    error::Error,
    ffi::{CStr, CString},
    mem, ptr,
    str::FromStr,
    time::{Duration, Instant},
};

use sdl2::{
    event::Event,
    keyboard::Keycode,
    mouse::{MouseButton, MouseUtil},
    video::{GLContext, GLProfile, Window},
};

use crate::{
    shaders::{
        GL20_FRAGMENT_SHADER, GL20_VERTEX_SHADER, GL33_FRAGMENT_SHADER, GL33_VERTEX_SHADER,
        GLES2_FRAGMENT_SHADER, GLES2_VERTEX_SHADER,
    },
    wayland_capture::WaylandCapturer,
};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

mod shaders;
mod wayland_capture;

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

trait ScreenCapturer {
    fn capture(&self) -> Result<Screenshot>;
}

struct Config {
    backend: RenderBackend,
}

impl Config {
    fn parse(args: impl IntoIterator<Item = String>) -> Result<Self> {
        let mut backend = RenderBackend::default();
        let mut args = args.into_iter();

        while let Some(arg) = args.next() {
            if arg == "--help" || arg == "-h" {
                return Err("usage: panium [--backend <gles2|gl33|gl20>]".into());
            }

            if arg == "--backend" {
                let value = args
                    .next()
                    .ok_or("--backend requires one of: gles2, gl33, gl20")?;
                backend = value.parse()?;
                continue;
            }

            return Err(format!("unknown argument: {arg}").into());
        }

        Ok(Self { backend })
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum RenderBackend {
    Gles2,
    #[default]
    Gl33,
    Gl20,
}

impl RenderBackend {
    fn configure_gl(self, gl_attr: &sdl2::video::gl_attr::GLAttr<'_>) {
        match self {
            Self::Gles2 => {
                gl_attr.set_context_profile(GLProfile::GLES);
                gl_attr.set_context_version(2, 0);
            }
            Self::Gl33 => {
                gl_attr.set_context_profile(GLProfile::Core);
                gl_attr.set_context_version(3, 3);
            }
            Self::Gl20 => {
                gl_attr.set_context_profile(GLProfile::Compatibility);
                gl_attr.set_context_version(2, 0);
            }
        }
    }

    fn shaders(self) -> (&'static str, &'static str) {
        match self {
            Self::Gles2 => (GLES2_VERTEX_SHADER, GLES2_FRAGMENT_SHADER),
            Self::Gl33 => (GL33_VERTEX_SHADER, GL33_FRAGMENT_SHADER),
            Self::Gl20 => (GL20_VERTEX_SHADER, GL20_FRAGMENT_SHADER),
        }
    }

    fn uses_vertex_arrays(self) -> bool {
        self == Self::Gl33
    }
}

impl FromStr for RenderBackend {
    type Err = Box<dyn Error>;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "gles2" => Ok(Self::Gles2),
            "gl33" => Ok(Self::Gl33),
            "gl20" => Ok(Self::Gl20),
            other => {
                Err(format!("unsupported backend '{other}', expected gles2, gl33, or gl20").into())
            }
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
    let config = Config::parse(env::args().skip(1))?;
    println!("Backend selected: {:?}", config.backend);

    let screenshot = WaylandCapturer.capture()?;
    show_zoom_window(screenshot, config.backend)
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
    fn new(screenshot: Screenshot, backend: RenderBackend) -> Result<Self> {
        let sdl = sdl2::init()?;
        let video = sdl.video()?;
        let gl_attr = video.gl_attr();
        backend.configure_gl(&gl_attr);
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

        let gl = GlResources::new(&screenshot, backend)?;
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

fn show_zoom_window(screenshot: Screenshot, backend: RenderBackend) -> Result<()> {
    let mut viewer = Viewer::new(screenshot, backend)?;
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
    fn new(screenshot: &Screenshot, backend: RenderBackend) -> Result<Self> {
        let program = create_program(backend)?;
        let mut vao = 0;
        let mut vbo = 0;
        let mut texture = 0;

        unsafe {
            if backend.uses_vertex_arrays() {
                gl::GenVertexArrays(1, &mut vao);
                gl::BindVertexArray(vao);
            }
            gl::GenBuffers(1, &mut vbo);
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

        let image_texture_uniform =
            unsafe { gl::GetUniformLocation(program, CString::new("image_texture")?.as_ptr()) };
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

        unsafe {
            gl::UseProgram(program);
            gl::Uniform1i(image_texture_uniform, 0);
        }

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
            if self.vao != 0 {
                gl::BindVertexArray(self.vao);
            }
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
            if self.vao != 0 {
                gl::DeleteVertexArrays(1, &self.vao);
            }
            gl::DeleteProgram(self.program);
        }
    }
}

fn create_program(backend: RenderBackend) -> Result<u32> {
    let (vertex_source, fragment_source) = backend.shaders();
    let vertex_shader = compile_shader(gl::VERTEX_SHADER, vertex_source)?;
    let fragment_shader = compile_shader(gl::FRAGMENT_SHADER, fragment_source)?;
    let program = unsafe { gl::CreateProgram() };

    unsafe {
        gl::AttachShader(program, vertex_shader);
        gl::AttachShader(program, fragment_shader);
        gl::BindAttribLocation(program, 0, CString::new("position")?.as_ptr());
        gl::BindAttribLocation(program, 1, CString::new("tex_coord")?.as_ptr());
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
