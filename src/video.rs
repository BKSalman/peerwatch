use ffmpeg_next as ffmpeg;

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum PixelFormat {
    Rgba,
    Bgra,
}

impl Default for PixelFormat {
    fn default() -> Self {
        PixelFormat::Rgba
    }
}

#[derive(Debug)]
pub struct VideoFormat {
    pub pixel_format: PixelFormat,
    pub dimensions: [u32; 2],
}

#[derive(Debug)]
pub struct VideoFrame {
    format: VideoFormat,
    data: Vec<u8>,
}

impl VideoFrame {
    pub fn width(&self) -> u32 {
        self.format.dimensions[0]
    }
    pub fn height(&self) -> u32 {
        self.format.dimensions[1]
    }
    pub fn data(&self) -> &[u8] {
        &self.data
    }
}

impl TryFrom<ffmpeg::frame::Video> for VideoFrame {
    type Error = anyhow::Error;

    fn try_from(value: ffmpeg::frame::Video) -> anyhow::Result<Self> {
        let pixel_format = match value.format() {
            ffmpeg_next::format::Pixel::RGBA => PixelFormat::Rgba,
            ffmpeg_next::format::Pixel::BGRA => PixelFormat::Bgra,
            pixel_format => {
                return Err(anyhow::anyhow!(
                    "unsupported pixel format: {pixel_format:?}"
                ));
            }
        };

        Ok(Self {
            format: VideoFormat {
                pixel_format,
                dimensions: [value.width(), value.height()],
            },
            data: value.data(0).to_vec(),
        })
    }
}
