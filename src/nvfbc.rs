use std::{any::Any, sync::mpsc};

use crate::{
    frame::{DrmFormat, FrameFormat, MemPtrFrame, WlxFrame, DRM_FORMAT_ARGB8888},
    WlxCapture,
};
use nvfbc::system::CaptureMethod;
use nvfbc::{BufferFormat, SystemCapturer};
pub struct NVFBCCapture<U, R>
where
    U: Any + Send,
    R: Any + Send,
{
    sender: Option<mpsc::SyncSender<()>>,
    receiver: Option<mpsc::Receiver<R>>,
    _dummy: Option<Box<U>>,
}

impl<U, R> NVFBCCapture<U, R>
where
    U: Any + Send,
    R: Any + Send,
{
    pub fn new() -> Self {
        Self {
            sender: None,
            receiver: None,
            _dummy: None,
        }
    }
}

impl<U, R> WlxCapture<U, R> for NVFBCCapture<U, R>
where
    U: Any + Send + Clone,
    R: Any + Send,
{
    fn init(
        &mut self,
        _: &[DrmFormat],
        user_data: U,
        receive_callback: fn(&U, WlxFrame) -> Option<R>,
    ) {
        let (tx_frame, rx_frame) = std::sync::mpsc::sync_channel(4);
        let (tx_cmd, rx_cmd) = std::sync::mpsc::sync_channel(2);
        self.sender = Some(tx_cmd);
        self.receiver = Some(rx_frame);

        std::thread::spawn({
            move || {
                let mut capturer = SystemCapturer::new().expect("Failed to create capturer.");
                if capturer.start(BufferFormat::Bgra, 90).is_err() {
                    log::error!("Failed to create NvFBC Capturer");
                    return;
                };

                loop {
                    let monitor_name = "NvFBC Main Monitor"; //capturer.status().unwrap().outputs[0].name;
                    match rx_cmd.recv() {
                        Ok(_) => {
                            if let Ok(frame_info) =
                                capturer.next_frame(CaptureMethod::NoWaitIfNewFrame)
                            {
                                log::trace!("{:#?}", frame_info);

                                let memptr_frame = MemPtrFrame {
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

                                log::trace!("{} captured frame: {:#?}", monitor_name, frame_info);

                                let frame = WlxFrame::MemPtr(memptr_frame);
                                if let Some(r) = receive_callback(&user_data, frame) {
                                    match tx_frame.try_send(r) {
                                        Ok(_) => (),
                                        Err(mpsc::TrySendError::Full(_)) => {
                                            log::debug!("{}: channel full", monitor_name);
                                        }
                                        Err(mpsc::TrySendError::Disconnected(_)) => {
                                            log::warn!(
                                                "{}: capture thread channel closed (send)",
                                                monitor_name,
                                            );
                                            break;
                                        }
                                    }
                                }
                            } else {
                                log::debug!("{}: NvFBC capture failed", monitor_name);
                            }
                        }
                        Err(_) => {
                            log::warn!("{}: capture thread channel closed (recv)", monitor_name);
                            break;
                        }
                    }
                }
                log::warn!("NvFBC capture thread stopped");
            }
        });
    }
    fn is_ready(&self) -> bool {
        self.receiver.is_some()
    }
    fn supports_dmbuf(&self) -> bool {
        false
    }
    fn receive(&mut self) -> Option<R> {
        if let Some(rx) = self.receiver.as_ref() {
            return rx.try_iter().last();
        }
        None
    }
    fn pause(&mut self) {}
    fn resume(&mut self) {
        if let Some(rx) = self.receiver.as_ref() {
            log::debug!(
                "NVFBC: dropped {} old frames before resuming",
                rx.try_iter().count()
            );
        }
        self.request_new_frame();
    }
    fn request_new_frame(&mut self) {
        if let Some(sender) = &self.sender {
            if let Err(e) = sender.send(()) {
                log::debug!("Failed to send frame request: {}", e);
            }
        }
    }
}
