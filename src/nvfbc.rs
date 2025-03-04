use crate::{
    frame::{DrmFormat, FrameFormat, MemPtrFrame, WlxFrame, DRM_FORMAT_ARGB8888},
    WlxCapture,
};
use nvfbc::system::CaptureMethod;
use nvfbc::{BufferFormat, SystemCapturer};
pub struct NVFBCCapture {
    capturer: SystemCapturer,
    fps: u32,
    // frame: Option<Box<[u8]>>,
}

impl NVFBCCapture {
    pub fn new() -> Self {
        Self {
            capturer: SystemCapturer::new().expect("Failed to create capturer."),
            fps: 90,
            // frame: None,
        }
    }
}

impl WlxCapture for NVFBCCapture {
    fn init(&mut self, dmabuf_formats: &[DrmFormat]) {
        let _ = self.capturer.start(BufferFormat::Bgra, self.fps);
    }

    fn is_ready(&self) -> bool {
        if let Ok(status) = self.capturer.status() {
            return status.is_capture_possible;
        }
        false
    }

    fn supports_dmbuf(&self) -> bool {
        false
    }

    fn receive(&mut self) -> Option<WlxFrame> {
        match self.capturer.status() {
            Ok(status) if !status.is_capture_possible => {
                return None;
            }
            Ok(_status) => {}
            Err(_) => return None,
        }
        if let Ok(frame_info) = self.capturer.next_frame(CaptureMethod::NoWait) {
            log::trace!("{:#?}", frame_info);

            let memptr = MemPtrFrame {
                format: FrameFormat {
                    width: frame_info.width,
                    height: frame_info.height,
                    fourcc: DRM_FORMAT_ARGB8888.into(),
                    ..Default::default()
                },
                ptr: frame_info.buffer.as_ptr() as _,
                size: frame_info.buffer.len(),
                mouse: None,
            };

            return Some(WlxFrame::MemPtr(memptr));
        }
        None
    }

    fn pause(&mut self) {
        self.capturer.stop().expect("Failed to stop capture.");
    }

    fn resume(&mut self) {
        let _ = self.capturer.start(BufferFormat::Bgra, self.fps);
    }

    fn request_new_frame(&mut self) {}
}
