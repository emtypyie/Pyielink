use openh264::decoder::{Decoder, DecodedYUV};
use openh264::OpenH264API;
use openh264::formats::YUVSource;
use wgpu::{Device, Queue, Texture, TextureDescriptor, TextureDimension, TextureFormat, TextureUsages, Extent3d, ImageCopyTexture, Origin3d, ImageDataLayout};
use anyhow::Result;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Target frame interval for 30 FPS
const TARGET_FPS: u32 = 30;
const TARGET_FRAME_INTERVAL_MS: u64 = 1000 / TARGET_FPS as u64;
const MAX_QUEUE_SIZE: usize = 10;

/// A video frame with its presentation timestamp
#[derive(Debug)]
struct Frame {
    yuv: Option<DecodedYUV<'static>>,
    pts: u64, // milliseconds since stream start
}

impl Clone for Frame {
    fn clone(&self) -> Self {
        // We can't truly clone DecodedYUV, so we create a new Frame with empty yuv
        // The actual frame data will be moved when needed
        Self {
            yuv: None,
            pts: self.pts,
        }
    }
}

/// MPEG-TS packet parser for extracting PTS
struct TsParser {
    buffer: Vec<u8>,
}

impl TsParser {
    fn new() -> Self {
        Self { buffer: Vec::new() }
    }

    /// Feed MPEG-TS data and extract frames with PTS
    fn feed(&mut self, data: &[u8]) -> Vec<(Vec<u8>, u64)> {
        self.buffer.extend_from_slice(data);
        let mut frames = Vec::new();
        
        // MPEG-TS packets are 188 bytes each
        while self.buffer.len() >= 188 {
            // Copy packet data to avoid borrow checker issues
            let packet = self.buffer[..188].to_vec();
            self.buffer.drain(..188);
            
            if let Some((pes_data, pts)) = Self::parse_ts_packet(&packet) {
                frames.push((pes_data, pts));
            }
        }
        
        frames
    }

    /// Parse a single MPEG-TS packet, return PES payload and PTS if present
    fn parse_ts_packet(packet: &[u8]) -> Option<(Vec<u8>, u64)> {
        if packet.len() < 188 {
            return None;
        }
        
        // Check sync byte
        if packet[0] != 0x47 {
            return None;
        }
        
        // Parse header
        let payload_unit_start = (packet[1] & 0x40) != 0;
        let pid = ((packet[1] & 0x1F) as u16) << 8 | packet[2] as u16;
        let adaptation_field_control = (packet[3] >> 4) & 0x03;
        let has_payload = adaptation_field_control & 0x01 != 0;
        
        if !has_payload {
            return None;
        }
        
        let payload_start = 4;
        let adaptation_field_length = if adaptation_field_control & 0x02 != 0 {
            packet[4] as usize + 1
        } else {
            0
        };
        
        let payload_offset = payload_start + adaptation_field_length;
        if payload_offset >= 188 {
            return None;
        }
        
        let payload = &packet[payload_offset..188];
        
        // Parse PES header if this is a video stream (PID typically 0x1011 for video)
        if pid == 0x1011 || payload_unit_start {
            if let Some((pes_payload, pts)) = Self::parse_pes(payload) {
                return Some((pes_payload.to_vec(), pts));
            }
        }
        
        // Return raw payload if no PES parsing
        Some((payload.to_vec(), 0))
    }

    /// Parse PES packet to extract PTS
    fn parse_pes(payload: &[u8]) -> Option<(&[u8], u64)> {
        if payload.len() < 6 {
            return None;
        }
        
        // Check PES start code: 0x00 0x00 0x01
        if payload[0] != 0x00 || payload[1] != 0x00 || payload[2] != 0x01 {
            return None;
        }
        
        let stream_id = payload[3];
        if stream_id < 0xE0 || stream_id > 0xEF {
            // Not a video stream
            return None;
        }
        
        let pes_packet_length = ((payload[4] as u16) << 8) | payload[5] as u16;
        
        if payload.len() < 9 {
            return None;
        }
        
        let flags = payload[7];
        let header_data_length = payload[8] as usize;
        
        if payload.len() < 9 + header_data_length {
            return None;
        }
        
        let header_data = &payload[9..9 + header_data_length];
        
        // Check for PTS (bit 6 of flags)
        let has_pts = (flags & 0x80) != 0;
        let has_dts = (flags & 0x40) != 0;
        
        let mut pts = 0u64;
        if has_pts && header_data.len() >= 5 {
            pts = Self::parse_pts(&header_data[..5]);
        }
        
        let pes_data_start = 9 + header_data_length;
        if payload.len() <= pes_data_start {
            return Some((&[], pts));
        }
        
        Some((&payload[pes_data_start..], pts))
    }

    /// Parse 33-bit PTS from 5 bytes
    fn parse_pts(bytes: &[u8]) -> u64 {
        let pts = ((bytes[0] as u64 & 0x0E) << 29) |
                  ((bytes[1] as u64) << 22) |
                  ((bytes[2] as u64 & 0xFE) << 14) |
                  ((bytes[3] as u64) << 7) |
                  ((bytes[4] as u64 & 0xFE) >> 1);
        pts
    }
}

/// Frame queue with pacing logic
struct FrameQueue {
    frames: VecDeque<Frame>,
    next_frame_time: Option<Instant>,
    last_presented_pts: u64,
    frame_interval_ms: u64,
    dropped_frames: u64,
    duplicated_frames: u64,
}

impl FrameQueue {
    fn new(target_fps: u32) -> Self {
        Self {
            frames: VecDeque::with_capacity(MAX_QUEUE_SIZE),
            next_frame_time: None,
            last_presented_pts: 0,
            frame_interval_ms: 1000 / target_fps as u64,
            dropped_frames: 0,
            duplicated_frames: 0,
        }
    }

    fn push(&mut self, frame: Frame) {
        // Drop frames that are too old (more than 2 frame intervals behind)
        let now = Instant::now();
        if let Some(front) = self.frames.front() {
            let frame_age_ms = (front.pts as i64 - self.last_presented_pts as i64).abs();
            if frame_age_ms > self.frame_interval_ms as i64 * 3 {
                self.frames.pop_front();
                self.dropped_frames += 1;
            }
        }
        
        if self.frames.len() < MAX_QUEUE_SIZE {
            self.frames.push_back(frame);
        } else {
            // Queue full, drop oldest
            self.frames.pop_front();
            self.frames.push_back(frame);
            self.dropped_frames += 1;
        }
    }

    /// Get the next frame to present based on timing
    fn next_frame(&mut self) -> Option<Frame> {
        let now = Instant::now();
        
        if self.frames.is_empty() {
            return None;
        }
        
        // Initialize next_frame_time on first call
        if self.next_frame_time.is_none() {
            self.next_frame_time = Some(now);
        }
        
        let target_time = self.next_frame_time.unwrap();
        
        // Check if it's time to present the next frame
        if now >= target_time {
            if let Some(frame) = self.frames.pop_front() {
                self.last_presented_pts = frame.pts;
                self.next_frame_time = Some(target_time + Duration::from_millis(self.frame_interval_ms));
                
                // If we're behind schedule, try to catch up by dropping frames
                while self.frames.len() > 2 {
                    let next_pts = self.frames.front().map(|f| f.pts).unwrap_or(0);
                    let expected_pts = self.last_presented_pts + self.frame_interval_ms;
                    if next_pts < expected_pts {
                        self.frames.pop_front();
                        self.dropped_frames += 1;
                    } else {
                        break;
                    }
                }
                
                return Some(frame);
            }
        }
        
        // If we're ahead, check if we should duplicate the last frame
        if self.frames.len() <= 1 && now < target_time {
            // We're ahead of schedule, return None (will duplicate last frame on next call)
            return None;
        }
        
        None
    }

    fn get_duplicated_frame(&mut self) -> Option<Frame> {
        // Duplicate the last presented frame
        if self.last_presented_pts > 0 {
            // Create a dummy frame with incremented PTS
            let duplicated_pts = self.last_presented_pts + self.frame_interval_ms;
            // We can't easily duplicate the YUV data without keeping it around
            // For now, return None to indicate we need to wait for a real frame
            None
        } else {
            None
        }
    }

    fn stats(&self) -> (u64, u64) {
        (self.dropped_frames, self.duplicated_frames)
    }
}

/// Enhanced video decoder with frame pacing
pub struct VideoDecoder {
    decoder: Decoder,
    ts_parser: TsParser,
    frame_queue: FrameQueue,
    width: u32,
    height: u32,
    texture: Option<wgpu::Texture>,
    pending_yuv: Option<DecodedYUV<'static>>,
    frame_ready: bool,
    first_pts_received: bool,
}

impl VideoDecoder {
    pub fn new() -> Result<Self> {
        let api = OpenH264API::from_source();
        let decoder = Decoder::new(api)?;
        Ok(Self {
            decoder,
            ts_parser: TsParser::new(),
            frame_queue: FrameQueue::new(TARGET_FPS),
            width: 0,
            height: 0,
            texture: None,
            pending_yuv: None,
            frame_ready: false,
            first_pts_received: false,
        })
    }

    /// Feed MPEG-TS data, parse PTS, decode frames, and queue for presentation
    pub fn feed(&mut self, data: &[u8]) -> Result<bool> {
        let mut any_frame = false;
        
        // Parse MPEG-TS packets
        let frames = self.ts_parser.feed(data);
        
        for (pes_data, pts) in frames {
            if !pes_data.is_empty() {
                if !self.first_pts_received && pts > 0 {
                    self.first_pts_received = true;
                }
                
                // Decode the PES payload (H.264 NAL units)
                match self.decoder.decode(&pes_data)? {
                    Some(frame) => {
                        self.width = frame.width() as u32;
                        self.height = frame.height() as u32;
                        
                        let yuv = unsafe { std::mem::transmute(frame) };
                        self.frame_queue.push(Frame { yuv, pts });
                        any_frame = true;
                    }
                    None => {}
                }
            } else if pts > 0 && !self.first_pts_received {
                // Just a PTS update
                self.first_pts_received = true;
            }
        }
        
        Ok(any_frame)
    }

    /// Get the next frame to present based on pacing
    pub fn next_frame(&mut self) -> Option<DecodedYUV<'static>> {
        if let Some(frame) = self.frame_queue.next_frame() {
            self.frame_ready = true;
            Some(unsafe { std::mem::transmute(frame.yuv) })
        } else {
            None
        }
    }

    /// Take a frame immediately (for non-paced rendering)
    pub fn take_frame(&mut self) -> Option<DecodedYUV<'static>> {
        if self.frame_ready {
            self.frame_ready = false;
            self.pending_yuv.take()
        } else if let Some(frame) = self.frame_queue.frames.pop_front() {
            Some(unsafe { std::mem::transmute(frame.yuv) })
        } else {
            None
        }
    }

    pub fn upload_texture(&mut self, device: &Device, queue: &Queue) -> anyhow::Result<bool> {
        if let Some(frame) = self.next_frame() {
            let texture = self.create_texture(device, queue, &frame)?;
            self.texture = Some(texture);
            return Ok(true);
        }
        Ok(false)
    }

    fn create_texture(&self, device: &Device, queue: &Queue, frame: &DecodedYUV) -> anyhow::Result<wgpu::Texture> {
        let width = frame.width() as u32;
        let height = frame.height() as u32;
        let y_stride = frame.y_stride() as u32;

        let texture = device.create_texture(&TextureDescriptor {
            label: Some("video-frame"),
            size: Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: TextureDimension::D2,
            format: TextureFormat::R8Unorm,
            usage: TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST,
            view_formats: &[],
        });

        queue.write_texture(
            ImageCopyTexture {
                texture: &texture,
                mip_level: 0,
                origin: Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            frame.y(),
            ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(y_stride),
                rows_per_image: Some(height),
            },
            Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );

        Ok(texture)
    }

    pub fn get_texture(&self) -> Option<&wgpu::Texture> {
        self.texture.as_ref()
    }

    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn frame_stats(&self) -> (u64, u64) {
        self.frame_queue.stats()
    }
}