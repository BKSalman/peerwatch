use ffmpeg_next::{self as ffmpeg, codec::Context};

use ffmpeg::{Packet, frame::Audio as FfmpegAudioFrame};

pub struct AudioDecoder {
    decoder: ffmpeg_next::decoder::Audio,
    resampler: Option<ffmpeg::software::resampling::Context>,
    config: cpal::StreamConfig,
    decoded_frame: FfmpegAudioFrame,
    resampled_frame: FfmpegAudioFrame,
}

impl AudioDecoder {
    pub fn new(config: &cpal::StreamConfig, context: Context) -> Self {
        let decoder = context.decoder().audio().unwrap();
        Self {
            config: config.clone(),
            decoder,
            decoded_frame: FfmpegAudioFrame::empty(),
            resampled_frame: FfmpegAudioFrame::empty(),
            resampler: None,
        }
    }

    pub fn push_packet(&mut self, packet: Packet) -> anyhow::Result<()> {
        self.decoder.send_packet(&packet)?;
        Ok(())
    }

    pub fn flush(&mut self) {
        self.decoder.flush();
    }

    pub fn pop_samples(&mut self) -> anyhow::Result<Option<&[f32]>> {
        match self.decoder.receive_frame(&mut self.decoded_frame) {
            Ok(()) => {
                let resampler = if let Some(resampler) = &mut self.resampler {
                    resampler
                } else {
                    let target_channel_layout = match self.config.channels {
                        1 => ffmpeg_next::ChannelLayout::MONO,
                        2 => ffmpeg_next::ChannelLayout::STEREO,
                        c => anyhow::bail!("{c} audio channels is not supported"),
                    };
                    let target_sample_format = ffmpeg_next::util::format::sample::Sample::F32(
                        ffmpeg_next::util::format::sample::Type::Packed,
                    );

                    let new_resampler = ffmpeg::software::resampling::Context::get(
                        self.decoder.format(),
                        self.decoder.channel_layout(),
                        self.decoder.rate(),
                        target_sample_format,
                        target_channel_layout,
                        self.config.sample_rate,
                    )?;
                    self.resampler = Some(new_resampler);
                    self.resampler.as_mut().unwrap()
                };

                resampler
                    .run(&self.decoded_frame, &mut self.resampled_frame)
                    .unwrap();

                let frame = &self.resampled_frame;
                let expected_bytes =
                    frame.samples() * frame.channels() as usize * core::mem::size_of::<f32>();

                Ok(Some(bytemuck::cast_slice(&frame.data(0)[..expected_bytes])))
            }
            Err(ffmpeg::Error::Eof) => Ok(None),
            Err(ffmpeg::Error::Other { errno: 11 }) => Ok(None), // EAGAIN - need more input
            Err(err) => Err(err.into()),
        }
    }
}
