use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use rusty_opus::OpusDecoder;
use rodio::{OutputStream, OutputStreamHandle, Sink};
use cpal::traits::{HostTrait, DeviceTrait, StreamTrait};
use crate::creds::hash_password;

const OPUS_FRAME_SIZE: usize = 960; // 20ms at 48kHz
const OPUS_SAMPLE_RATE: u32 = 48000;
const OPUS_CHANNELS: usize = 1; // mono for voice

pub struct AudioPlayer {
    decoder: Arc<Mutex<OpusDecoder>>,
    _stream: Arc<OutputStream>,
    stream_handle: Arc<OutputStreamHandle>,
    sink: Arc<Mutex<Sink>>,
    running: Arc<std::sync::atomic::AtomicBool>,
}

impl AudioPlayer {
    pub fn new() -> Result<Self, String> {
        let decoder = OpusDecoder::new(OPUS_SAMPLE_RATE as i32, OPUS_CHANNELS)
            .map_err(|e| format!("Failed to create Opus decoder: {}", e))?;

        let (_stream, stream_handle) = OutputStream::try_default()
            .map_err(|e| format!("Failed to create audio output stream: {}", e))?;

        let sink = Sink::try_new(&stream_handle)
            .map_err(|e| format!("Failed to create audio sink: {}", e))?;

        Ok(Self {
            decoder: Arc::new(Mutex::new(decoder)),
            _stream: Arc::new(_stream),
            stream_handle: Arc::new(stream_handle),
            sink: Arc::new(Mutex::new(sink)),
            running: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        })
    }

    pub fn start(&self) {
        self.running.store(true, std::sync::atomic::Ordering::Relaxed);
        self.sink.lock().unwrap().play();
    }

    pub fn stop(&self) {
        self.running.store(false, std::sync::atomic::Ordering::Relaxed);
        self.sink.lock().unwrap().pause();
    }

    pub fn is_running(&self) -> bool {
        self.running.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn feed(&self, opus_data: &[u8]) -> Result<(), String> {
        if !self.running.load(std::sync::atomic::Ordering::Relaxed) {
            return Ok(());
        }

        // rusty-opus decodes to f32 planar format
        let mut pcm_buffer = vec![0.0f32; OPUS_FRAME_SIZE * OPUS_CHANNELS];
        let frame_size = {
            let mut decoder = self.decoder.lock().map_err(|e| e.to_string())?;
            decoder.decode(opus_data, OPUS_FRAME_SIZE, &mut pcm_buffer[..])
                .map_err(|e| format!("Opus decode error: {}", e))?
        };

        if frame_size > 0 {
            // Convert f32 planar to i16 interleaved for rodio
            let samples: Vec<i16> = pcm_buffer[..frame_size * OPUS_CHANNELS]
                .iter()
                .map(|&x| (x.clamp(-1.0, 1.0) * 32767.0) as i16)
                .collect();
            let source = rodio::buffer::SamplesBuffer::new(OPUS_CHANNELS as u16, OPUS_SAMPLE_RATE, samples);
            self.sink.lock().unwrap().append(source);
        }

        Ok(())
    }
}

pub struct AudioCapture {
    running: Arc<std::sync::atomic::AtomicBool>,
    _thread: Option<thread::JoinHandle<()>>,
}

impl AudioCapture {
    pub fn new() -> Self {
        Self {
            running: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            _thread: None,
        }
    }

    pub fn start(&mut self, _sample_rate: u32) {
        if self.running.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        self.running.store(true, std::sync::atomic::Ordering::Relaxed);

        let running = Arc::clone(&self.running);
        self._thread = Some(thread::spawn(move || {
            let host = cpal::default_host();
            let device = host.default_input_device().expect("no input device");
            let config = device.default_input_config().expect("failed to get default input config");
            
            let err_fn = |err| eprintln!("Audio capture error: {}", err);
            let _stream = device.build_input_stream(
                &config.into(),
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    // TODO: encode to Opus and send on AUDIO channel
                    // For now just keep the stream alive
                    let _ = data;
                },
                err_fn,
            ).expect("failed to build input stream");

            while running.load(std::sync::atomic::Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(100));
            }
        }));
    }

    pub fn stop(&mut self) {
        self.running.store(false, std::sync::atomic::Ordering::Relaxed);
        if let Some(thread) = self._thread.take() {
            let _ = thread.join();
        }
    }

    pub fn is_running(&self) -> bool {
        self.running.load(std::sync::atomic::Ordering::Relaxed)
    }
}

pub struct AudioManager {
    player: Option<AudioPlayer>,
    capture: Option<AudioCapture>,
}

impl AudioManager {
    pub fn new() -> Self {
        Self {
            player: None,
            capture: None,
        }
    }

    pub fn init_playback(&mut self) -> Result<(), String> {
        if self.player.is_none() {
            self.player = Some(AudioPlayer::new()?);
        }
        Ok(())
    }

    pub fn start_playback(&mut self) {
        if let Some(ref p) = self.player {
            p.start();
        }
    }

    pub fn stop_playback(&mut self) {
        if let Some(ref p) = self.player {
            p.stop();
        }
    }

    pub fn feed_audio(&self, data: &[u8]) -> Result<(), String> {
        if let Some(ref p) = self.player {
            p.feed(data)?;
        }
        Ok(())
    }

    pub fn init_capture(&mut self) {
        if self.capture.is_none() {
            self.capture = Some(AudioCapture::new());
        }
    }

    pub fn start_capture(&mut self) {
        if let Some(ref mut c) = self.capture {
            c.start(OPUS_SAMPLE_RATE);
        }
    }

    pub fn stop_capture(&mut self) {
        if let Some(ref mut c) = self.capture {
            c.stop();
        }
    }
}