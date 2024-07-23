use eframe::glow;
use anyhow::Result;
use egui::{Color32, ColorImage, TextureHandle, TextureId, Vec2};
use ffmpeg::media::Type;
use ffmpeg::util::frame::Video;
use ffmpeg::software::scaling::{Context, Flags};
use ffmpeg::format::{input, context::Input};
use log::error;
use std::thread;
use std::sync::Arc;
use egui::mutex::Mutex;


#[derive(Debug, Clone, Copy)]
pub enum State {
    Stopped,
    Paused,
    Playing,
    EndOfFile
}

pub struct Player {
    current_location: String,
    texture: TextureHandle,
    ctx: egui::Context,
    play_thread: Option<thread::JoinHandle<Result<()>>>,
    state: Arc<Mutex<State>>,
    thread_stop: Arc<Mutex<bool>>,
    ready_to_show: Arc<Mutex<bool>>,
    video_res: Arc<Mutex<Vec2>>,
}

impl Player {
    pub fn new(ctx: &egui::Context) -> Self {
        let texture_handle = ctx.load_texture("video", ColorImage::example(), Default::default());

        Self { 
            current_location: String::new(),
            ctx: ctx.clone(),
            texture: texture_handle,
            play_thread: None,
            state: Arc::new(Mutex::new(State::Stopped)),
            thread_stop: Arc::new(Mutex::new(false)),
            ready_to_show: Arc::new(Mutex::new(false)),
            video_res: Arc::new(Mutex::new(Vec2::new(0.0, 0.0)))
        }
    }

    pub fn texture(&self) -> TextureId {
        self.texture.id()
    }

    pub fn size(&self) -> Vec2 {
        (*self.video_res.lock()).clone()
    }

    pub fn ready_to_show(&self) -> bool {
        *self.ready_to_show.lock()
    }

    pub fn state(&self) -> State {
        *self.state.lock()
    }

    pub fn start(&mut self, location: &str) {
        if location != self.current_location {
            self.stop();
        }
        if self.play_thread.is_some() { 
            return
        }
        
        let location = location.to_owned();
        self.current_location = location.clone();
        
        *self.thread_stop.lock() = false;
        *self.state.lock() = State::Playing;
        let ctx = self.ctx.clone();
        let thread_stop = self.thread_stop.clone();
        let state = self.state.clone();
        let mut texture = self.texture.clone();
        let ready_to_show = self.ready_to_show.clone();
        let video_res = self.video_res.clone();
        self.play_thread = Some(thread::spawn(move || {
            let mut input = input(&location)?;
            let stream = input.streams().best(Type::Video).ok_or(ffmpeg::Error::StreamNotFound)?;
            let video_index = stream.index();
            let ctx_decoder = ffmpeg::codec::context::Context::from_parameters(stream.parameters())?;
            let mut decoder = ctx_decoder.decoder().video()?;
            let frame_rate = stream.avg_frame_rate().numerator() as f64 / stream.avg_frame_rate().denominator() as f64;
            let wait_duration = std::time::Duration::from_millis((1000.0 / frame_rate) as u64);
            *video_res.lock() = Vec2::new(decoder.width() as f32, decoder.height() as f32);
            let mut scaler = Context::get(decoder.format(), decoder.width(), decoder.height(), ffmpeg::format::Pixel::RGB24, decoder.width(), decoder.height(), Flags::BILINEAR).unwrap();
            loop {
                if matches!(*state.lock(), State::Paused) {
                    std::thread::sleep(std::time::Duration::from_millis(200));
                    continue;
                }
                let mut frame = Video::empty();
                match decoder.receive_frame(&mut frame) {
                    Ok(_) => {
                        let mut rgb_frame = Video::empty();
                        match scaler.run(&frame, &mut rgb_frame) {
                            Ok(_) => {
                                let image = video_frame_to_image(rgb_frame);
                                texture.set(image, Default::default());
                                *ready_to_show.lock() = true;
                                ctx.request_repaint()
                            },
                            Err(err) => error!("scaler error {:?}", err)
                        }
                    }
                    Err(err) => {
                        if matches!(err, ffmpeg::Error::Eof) {
                            *state.lock() = State::EndOfFile;
                            *ready_to_show.lock() = false;
                            ctx.request_repaint();
                            break
                        }
                        if let ffmpeg::Error::Other { errno } = err {
                            if errno == ffmpeg::error::EAGAIN {
                                if let Some((stream, packet)) = input.packets().next() {
                                    if stream.index() == video_index {
                                        decoder.send_packet(&packet);
                                    }
                                }
                                else {
                                    decoder.send_eof();
                                }
                                continue
                            }
                        }
                        error!("player error {:?}", err)
                    },
                }
                thread::sleep(wait_duration);
                if *thread_stop.lock() {
                    break
                }
            }
            Ok(())
        }));
    }

    pub fn stop(&mut self) {
        *self.state.lock() = State::Stopped;
        self.ctx.request_repaint();
        if let Some(th) = self.play_thread.take() {
            *self.thread_stop.lock() = true;
            let _ = th.join();
        }
        self.current_location = String::new();
    }
    pub fn pause(&mut self) {
        *self.state.lock() = State::Paused;
    }
    pub fn unpause(&mut self) {
        if self.play_thread.is_some() {
            *self.state.lock() = State::Playing;
        }
    }
}


fn video_frame_to_image(frame: Video) -> ColorImage {
    let size = [frame.width() as usize, frame.height() as usize];
    let data = frame.data(0);
    let stride = frame.stride(0);
    let pixel_size_bytes = 3;
    let byte_width: usize = pixel_size_bytes * frame.width() as usize;
    let height: usize = frame.height() as usize;
    let mut pixels = vec![];
    for line in 0..height {
        let begin = line * stride;
        let end = begin + byte_width;
        let data_line = &data[begin..end];
        pixels.extend(
            data_line
                .chunks_exact(pixel_size_bytes)
                .map(|p| Color32::from_rgb(p[0], p[1], p[2])),
        )
    }
    ColorImage { size, pixels }
}
