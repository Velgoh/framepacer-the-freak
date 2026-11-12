use egui::{ColorImage, Context, TextureHandle, TextureOptions};
use image::AnimationDecoder;
use image::codecs::gif::GifDecoder;
use std::io::Cursor;
use std::time::{Duration, Instant};

pub const KAINE_GIF_BYTES: &[u8] = include_bytes!("../assets/kaine-cry.gif");

pub struct GifPlayer {
    frames: Vec<(ColorImage, Duration)>,
    textures: Vec<TextureHandle>,
    current_index: usize,
    last_frame_time: Instant,
}

impl GifPlayer {
    pub fn load_from_bytes(bytes: &[u8]) -> Result<Self, String> {
        let decoder =
            GifDecoder::new(Cursor::new(bytes)).map_err(|e| format!("GIF decode error: {}", e))?;
        let raw_frames = decoder
            .into_frames()
            .collect_frames()
            .map_err(|e| format!("Collect frames error: {}", e))?;

        let mut frames = Vec::with_capacity(raw_frames.len());
        for frame in raw_frames {
            let (num, den) = frame.delay().numer_denom_ms();
            let ms = if den > 0 {
                (num as f64 / den as f64).round() as u64
            } else {
                100
            };
            let delay = Duration::from_millis(ms.max(16));
            let buffer = frame.into_buffer();
            let size = [buffer.width() as usize, buffer.height() as usize];
            let pixels = buffer.into_raw();
            let color_image = ColorImage::from_rgba_unmultiplied(size, &pixels);
            frames.push((color_image, delay));
        }

        Ok(Self {
            frames,
            textures: Vec::new(),
            current_index: 0,
            last_frame_time: Instant::now(),
        })
    }

    pub fn new_embedded() -> Self {
        Self::load_from_bytes(KAINE_GIF_BYTES).expect("Failed to load embedded kaine-cry.gif")
    }

    pub fn frame_count(&self) -> usize {
        self.frames.len()
    }

    pub fn prepare_textures(&mut self, ctx: &Context) {
        if self.textures.is_empty() && !self.frames.is_empty() {
            self.textures = self
                .frames
                .iter()
                .enumerate()
                .map(|(i, (img, _))| {
                    ctx.load_texture(
                        format!("kaine_frame_{}", i),
                        img.clone(),
                        TextureOptions::LINEAR,
                    )
                })
                .collect();
        }
    }

    pub fn update_and_get_texture(&mut self, ctx: &Context) -> Option<&TextureHandle> {
        if self.frames.is_empty() {
            return None;
        }

        self.prepare_textures(ctx);

        let now = Instant::now();
        let elapsed = now.duration_since(self.last_frame_time);
        let mut current_delay = self.frames[self.current_index].1;

        if elapsed >= current_delay {
            let mut remaining_elapsed = elapsed;
            while remaining_elapsed >= current_delay {
                remaining_elapsed -= current_delay;
                self.current_index = (self.current_index + 1) % self.frames.len();
                current_delay = self.frames[self.current_index].1;
            }
            self.last_frame_time = now.checked_sub(remaining_elapsed).unwrap_or(now);
        }

        let current_elapsed = now.duration_since(self.last_frame_time);
        let remaining = current_delay.saturating_sub(current_elapsed);
        ctx.request_repaint_after(remaining);

        self.textures.get(self.current_index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_embedded_gif_decoding() {
        let player = GifPlayer::new_embedded();
        assert!(
            player.frame_count() > 0,
            "Expected kaine-cry.gif to contain frames"
        );
        println!(
            "[GIF TEST] Loaded embedded kaine-cry.gif with {} frames",
            player.frame_count()
        );
    }
}
