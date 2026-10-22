//! Tells when a pixel of the screen shows a color, from the compositor's own
//! presentation time (wlr-screencopy): unlike polling with grim, neither the
//! start of a process nor a capture that missed the frame count.
//!
//! Usage: `probe OUTPUT X Y`, X and Y in the output's logical coordinates.
//! For each line `R G B` on stdin, captures the pixel frame after frame until
//! it is that color, within 8 per channel, and prints the presentation time of
//! that frame in nanoseconds of CLOCK_MONOTONIC, or `timeout` after 10 s.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, Write};
use std::os::unix::fs::FileExt;
use std::time::{Duration, Instant};

use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, WEnum, delegate_noop};
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_frame_v1::{
    self, ZwlrScreencopyFrameV1,
};
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1;

#[derive(Default)]
struct State {
    names: Vec<Option<String>>,
    /// The buffer the compositor asks for: format, width, height, stride.
    buffer: Option<(wl_shm::Format, u32, u32, u32)>,
    buffer_done: bool,
    ready: Option<u64>,
    failed: bool,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [output, x, y] = &args[..] else {
        eprintln!("usage: probe OUTPUT X Y");
        std::process::exit(2);
    };
    let (x, y): (i32, i32) = (x.parse().unwrap(), y.parse().unwrap());

    let conn = Connection::connect_to_env().expect("no Wayland compositor");
    let (globals, mut queue) = registry_queue_init::<State>(&conn).unwrap();
    let qh = queue.handle();
    let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).unwrap();
    let manager: ZwlrScreencopyManagerV1 = globals.bind(&qh, 1..=3, ()).expect("no wlr-screencopy");
    let outputs: Vec<wl_output::WlOutput> = globals
        .contents()
        .clone_list()
        .iter()
        .filter(|g| g.interface == "wl_output")
        .enumerate()
        .map(|(i, g)| {
            globals
                .registry()
                .bind(g.name, g.version.clamp(4, 4), &qh, i)
        })
        .collect();
    let mut state = State {
        names: vec![None; outputs.len()],
        ..State::default()
    };
    queue.roundtrip(&mut state).unwrap();
    let Some(i) = state
        .names
        .iter()
        .position(|n| n.as_deref() == Some(output))
    else {
        eprintln!("probe: no output {output}");
        std::process::exit(2);
    };
    let output = &outputs[i];

    let path = format!("/dev/shm/raterm-probe-{}", std::process::id());
    let file: File = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)
        .unwrap();
    std::fs::remove_file(&path).unwrap();
    file.set_len(4096).unwrap();
    let pool = shm.create_pool(std::os::fd::AsFd::as_fd(&file), 4096, &qh, ());
    let mut buffer: Option<(wl_buffer::WlBuffer, wl_shm::Format)> = None;

    let stdout = std::io::stdout();
    for line in std::io::stdin().lock().lines() {
        let want: Vec<u8> = line
            .unwrap()
            .split_whitespace()
            .map(|c| c.parse().unwrap())
            .collect();
        let end = Instant::now() + Duration::from_secs(10);
        let seen = loop {
            if Instant::now() > end {
                break None;
            }
            state.buffer_done = false;
            state.ready = None;
            state.failed = false;
            let frame = manager.capture_output_region(0, output, x, y, 1, 1, &qh, ());
            // Version 3 lists every buffer type before buffer_done.
            while state.buffer.is_none() || (manager.version() >= 3 && !state.buffer_done) {
                queue.blocking_dispatch(&mut state).unwrap();
            }
            let (format, w, h, stride) = state.buffer.unwrap();
            let (wl_buffer, _) = buffer.get_or_insert_with(|| {
                let b = pool.create_buffer(0, w as i32, h as i32, stride as i32, format, &qh, ());
                (b, format)
            });
            frame.copy(wl_buffer);
            while state.ready.is_none() && !state.failed {
                queue.blocking_dispatch(&mut state).unwrap();
            }
            frame.destroy();
            let Some(at) = state.ready else {
                continue;
            };
            let mut px = [0u8; 4];
            file.read_exact_at(&mut px, 0).unwrap();
            // 32-bit formats in little endian: B G R x, or R G B x.
            let rgb = match format {
                wl_shm::Format::Xbgr8888 | wl_shm::Format::Abgr8888 => [px[0], px[1], px[2]],
                _ => [px[2], px[1], px[0]],
            };
            if rgb.iter().zip(&want).all(|(a, b)| a.abs_diff(*b) <= 8) {
                break Some(at);
            }
        };
        let mut out = stdout.lock();
        match seen {
            Some(at) => writeln!(out, "{at}"),
            None => writeln!(out, "timeout"),
        }
        .unwrap();
        out.flush().unwrap();
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_screencopy_frame_v1::Event;
        match event {
            Event::Buffer {
                format: WEnum::Value(format),
                width,
                height,
                stride,
            } => state.buffer = Some((format, width, height, stride)),
            Event::BufferDone => state.buffer_done = true,
            Event::Ready {
                tv_sec_hi,
                tv_sec_lo,
                tv_nsec,
            } => {
                let secs = ((tv_sec_hi as u64) << 32) | tv_sec_lo as u64;
                state.ready = Some(secs * 1_000_000_000 + tv_nsec as u64);
            }
            Event::Failed => state.failed = true,
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, usize> for State {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        i: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event {
            state.names[*i] = Some(name);
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_buffer::WlBuffer);
delegate_noop!(State: wl_shm_pool::WlShmPool);
delegate_noop!(State: ZwlrScreencopyManagerV1);
