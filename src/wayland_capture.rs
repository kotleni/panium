use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    os::fd::AsFd,
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

use crate::{Result, ScreenCapturer, Screenshot};

pub struct WaylandCapturer;

impl ScreenCapturer for WaylandCapturer {
    fn capture(&self) -> Result<Screenshot> {
        capture_screenshot()
    }
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
    let bpp: u32 = match format {
        wl_shm::Format::Bgr888 | wl_shm::Format::Rgb888 => 3,
        _ => 4,
    };

    let mut rgba = vec![0; (width * height * 4) as usize];

    for y in 0..height {
        let source_y = if y_inverted { height - 1 - y } else { y };
        for x in 0..width {
            let source = (source_y * stride + x * bpp) as usize;
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
                wl_shm::Format::Bgr888 => [
                    pixels[source],
                    pixels[source + 1],
                    pixels[source + 2],
                    255,
                ],
                wl_shm::Format::Rgb888 => [
                    pixels[source + 2],
                    pixels[source + 1],
                    pixels[source],
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

delegate_registry!(CaptureState);
delegate_output!(CaptureState);
delegate_noop!(CaptureState: ignore wl_shm::WlShm);
delegate_noop!(CaptureState: ignore wl_shm_pool::WlShmPool);
delegate_noop!(CaptureState: ignore wl_buffer::WlBuffer);
