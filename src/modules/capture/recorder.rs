use std::thread::{self, JoinHandle};

use color_eyre::eyre::{self, ensure, eyre, Context};
use crossbeam_channel::{bounded, Receiver, Sender};
use rust_hawktracer::*;

use super::{
    muxer::{Muxer, MuxerInitError},
    opengl::{self, OpenGl},
    vulkan::{self, ExternalHandles, Vulkan},
    SoundCaptureMode,
};
use crate::utils::*;

pub struct Recorder {
    /// Video width.
    width: i32,

    /// Video height.
    height: i32,

    /// The target time base.
    time_base: f64,

    /// Difference, in video frames, between how much time passed in-game and how much video we
    /// output.
    video_remainder: f64,

    /// Difference, in seconds, between how much time passed in-game and how much audio we output.
    sound_remainder: f64,

    /// OpenGL state; might be missing if the capturing just started or just after an engine
    /// restart.
    opengl: Option<OpenGl>,

    /// Whether Vulkan has already acquired the last frame.
    acquired_image: bool,

    /// Vulkan recording and muxing thread.
    thread: JoinHandle<()>,

    /// Sender for messages to the thread.
    sender: Sender<MainToThread>,

    /// Receiver for messages from the thread.
    receiver: Receiver<ThreadToMain>,

    /// Error from the thread if it sent one.
    thread_error: Option<eyre::Report>,
}

#[derive(Debug)]
enum MainToThread {
    Finish,
    GiveExternalHandles,
    AcquireImage,
    Record { frames: usize },
    Audio(Vec<u8>),
}

#[derive(Debug)]
enum ThreadToMain {
    Error(eyre::Report),
    ExternalHandles(ExternalHandles),
    AcquiredImage,
}

impl Recorder {
    #[hawktracer(recorder_init)]
    pub unsafe fn init(
        width: i32,
        height: i32,
        fps: u64,
        filename: &str,
    ) -> eyre::Result<Recorder> {
        ensure!(
            width % 2 == 0 && height % 2 == 0,
            "can't handle odd game resolutions yet: {}×{}",
            width,
            height,
        );

        let vulkan =
            vulkan::init(width as u32, height as u32).wrap_err("error initalizing Vulkan")?;

        let time_base = 1. / fps as f64;

        let muxer = match Muxer::new(width as u64, height as u64, fps, filename) {
            Ok(muxer) => muxer,
            Err(err @ MuxerInitError::FfmpegSpawn(_)) => {
                return Err(err).wrap_err(
                    #[cfg(unix)]
                    "could not start ffmpeg. Make sure you have \
                    ffmpeg installed and present in PATH",
                    #[cfg(windows)]
                    "could not start ffmpeg. Make sure you have \
                    ffmpeg.exe in the Half-Life folder",
                );
            }
            Err(err) => {
                return Err(err).wrap_err("error initializing muxing");
            }
        };

        let (to_thread_sender, from_main_receiver) = bounded(2);
        let (to_main_sender, from_thread_receiver) = bounded(1);
        let thread =
            thread::spawn(move || thread(vulkan, muxer, to_main_sender, from_main_receiver));

        Ok(Recorder {
            width,
            height,
            time_base,
            video_remainder: 0.,
            sound_remainder: 0.,
            opengl: None,
            acquired_image: false,
            thread,
            sender: to_thread_sender,
            receiver: from_thread_receiver,
            thread_error: None,
        })
    }

    fn send_to_thread(&mut self, message: MainToThread) {
        if self.sender.send(message).is_ok() {
            // The happy path.
            return;
        }

        // The channel was closed. Try to get the error.
        while let Ok(message) = self.receiver.try_recv() {
            if let ThreadToMain::Error(err) = message {
                self.thread_error = Some(err);
            }
        }
    }

    fn recv_from_thread(&mut self) -> eyre::Result<ThreadToMain> {
        match self.receiver.recv() {
            Err(_) => Err(self
                .thread_error
                .take()
                .unwrap_or_else(|| eyre!("recording thread error"))),
            Ok(ThreadToMain::Error(err)) => Err(err),
            Ok(message) => Ok(message),
        }
    }

    #[hawktracer(initialize_opengl_capturing)]
    unsafe fn initialize_opengl_capturing(&mut self, marker: MainThreadMarker) -> eyre::Result<()> {
        self.send_to_thread(MainToThread::GiveExternalHandles);
        let external_handles = match self.recv_from_thread()? {
            ThreadToMain::ExternalHandles(handles) => handles,
            _ => unreachable!(),
        };

        self.opengl = Some(opengl::init(
            marker,
            self.width,
            self.height,
            external_handles.size,
            external_handles.external_image_frame_memory,
            external_handles.external_semaphore,
        )?);

        Ok(())
    }

    pub unsafe fn capture_opengl(&mut self, marker: MainThreadMarker) -> eyre::Result<()> {
        if self.opengl.is_none() {
            self.initialize_opengl_capturing(marker)?;
        }

        self.opengl.as_ref().unwrap().capture()
    }

    #[hawktracer(acquire_image_if_needed)]
    unsafe fn acquire_image_if_needed(&mut self) {
        if self.acquired_image {
            return;
        }

        let frames = (self.video_remainder + 0.5) as usize;
        if frames == 0 {
            return;
        }

        self.acquired_image = true;

        self.send_to_thread(MainToThread::AcquireImage);
    }

    #[hawktracer(record)]
    unsafe fn record(&mut self, frames: usize) -> eyre::Result<()> {
        assert!(self.acquired_image);

        // Must wait for this before OpenGL capture can run.
        assert!(matches!(
            self.recv_from_thread()?,
            ThreadToMain::AcquiredImage
        ));

        self.acquired_image = false;

        self.send_to_thread(MainToThread::Record { frames });

        Ok(())
    }

    #[hawktracer(record_last_frame)]
    pub unsafe fn record_last_frame(&mut self) -> eyre::Result<()> {
        // Push this frame as long as it takes up the most of the video frame.
        // Remainder is > -0.5 at all times.
        let frames = (self.video_remainder + 0.5) as usize;
        self.video_remainder -= frames as f64;

        if frames > 0 {
            self.record(frames)?;
        }

        Ok(())
    }

    pub fn time_passed(&mut self, time: f64) {
        self.video_remainder += time / self.time_base;
        self.sound_remainder += time;
        unsafe {
            self.acquire_image_if_needed();
        }
    }

    pub fn samples_to_capture(&mut self, samples_per_second: i32, mode: SoundCaptureMode) -> i32 {
        let samples = self.sound_remainder * samples_per_second as f64;
        let samples_rounded = match mode {
            SoundCaptureMode::Normal => samples.floor(),
            SoundCaptureMode::Remaining { extra } => {
                (samples + extra as f64 * samples_per_second as f64).ceil()
            }
        };

        self.sound_remainder = (samples - samples_rounded) / samples_per_second as f64;

        samples_rounded as i32
    }

    #[hawktracer(write_audio_frame)]
    pub fn write_audio_frame(&mut self, samples: Vec<u8>) {
        self.send_to_thread(MainToThread::Audio(samples));
    }

    #[hawktracer(recorder_finish)]
    pub fn finish(mut self) {
        self.send_to_thread(MainToThread::Finish);

        while let Ok(message) = self.receiver.recv() {
            if let ThreadToMain::Error(err) = message {
                self.thread_error = Some(err);
            }
        }

        self.thread.join().unwrap();

        if let Some(err) = self.thread_error {
            error!("recording thread error: {:?}", err);
        }
    }

    pub fn reset_opengl(&mut self) {
        self.opengl = None;
    }

    pub fn width(&self) -> i32 {
        self.width
    }

    pub fn height(&self) -> i32 {
        self.height
    }

    pub fn time_base(&self) -> f64 {
        self.time_base
    }
}

fn thread(vulkan: Vulkan, mut muxer: Muxer, s: Sender<ThreadToMain>, r: Receiver<MainToThread>) {
    while let Ok(message) = r.recv() {
        match process_message(&vulkan, &mut muxer, &s, message) {
            Ok(done) => {
                if done {
                    break;
                }
            }
            Err(err) => {
                s.send(ThreadToMain::Error(err)).unwrap();
                break;
            }
        }
    }

    muxer.close();
}

fn process_message(
    vulkan: &Vulkan,
    muxer: &mut Muxer,
    s: &Sender<ThreadToMain>,
    message: MainToThread,
) -> eyre::Result<bool> {
    match message {
        MainToThread::Finish => {
            return Ok(true);
        }
        MainToThread::GiveExternalHandles => {
            let handles = vulkan.external_handles()?;
            s.send(ThreadToMain::ExternalHandles(handles)).unwrap();
        }
        MainToThread::AcquireImage => {
            scoped_tracepoint!(_acquire);

            unsafe { vulkan.acquire_image() }?;

            s.send(ThreadToMain::AcquiredImage).unwrap();
        }
        MainToThread::Record { frames } => {
            scoped_tracepoint!(_record);

            unsafe { vulkan.convert_colors_and_mux(muxer, frames) }?;
        }
        MainToThread::Audio(samples) => {
            scoped_tracepoint!(_audio);

            muxer.write_audio_frame(&samples)?;
        }
    }

    Ok(false)
}
