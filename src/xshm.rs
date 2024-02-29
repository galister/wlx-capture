use std::{
    env,
    error::Error,
    sync::{
        mpsc::{self},
        Arc,
    },
};

use rxscreen::monitor::Monitor;

use crate::{
    frame::{DrmFormat, FrameFormat, MemPtrFrame, MouseMeta, WlxFrame, DRM_FORMAT_XRGB8888},
    WlxCapture,
};

pub struct XshmScreen {
    pub name: Arc<str>,
    pub monitor: Monitor,
}

pub struct XshmCapture {
    pub screen: Arc<XshmScreen>,
    sender: Option<mpsc::SyncSender<()>>,
    receiver: Option<mpsc::Receiver<WlxFrame>>,
}

impl XshmCapture {
    pub fn new(screen: Arc<XshmScreen>) -> Self {
        Self {
            screen,
            sender: None,
            receiver: None,
        }
    }

    pub fn get_monitors() -> Result<Vec<Arc<XshmScreen>>, Box<dyn Error>> {
        let display = env::var("DISPLAY")?;
        let Ok(d) = rxscreen::Display::new(display) else {
            return Err("X11: Failed to open display".into());
        };
        Ok(d.monitors()
            .into_iter()
            .enumerate()
            .map(|x| {
                Arc::new(XshmScreen {
                    name: x.1.name().replace("DisplayPort", "DP").into(),
                    monitor: x.1,
                })
            })
            .collect())
    }
}

impl WlxCapture for XshmCapture {
    fn init(&mut self, _: &[DrmFormat]) {
        let (tx_frame, rx_frame) = std::sync::mpsc::sync_channel(4);
        let (tx_cmd, rx_cmd) = std::sync::mpsc::sync_channel(2);
        self.sender = Some(tx_cmd);
        self.receiver = Some(rx_frame);

        std::thread::spawn({
            let monitor = self.screen.monitor.clone();
            move || {
                let display = env::var("DISPLAY").expect("DISPLAY not set");
                let Ok(d) = rxscreen::Display::new(display) else {
                    log::error!("{}: failed to open display", monitor.name());
                    return;
                };
                let Ok(shm) = d.shm().monitor(&monitor).build() else {
                    log::error!("{}: failed to create shm", monitor.name());
                    return;
                };

                loop {
                    match rx_cmd.recv() {
                        Ok(_) => {
                            if let Ok(image) = shm.capture() {
                                let size = unsafe { image.as_bytes().len() };
                                let memptr_frame = MemPtrFrame {
                                    format: FrameFormat {
                                        width: image.width() as _,
                                        height: image.height() as _,
                                        fourcc: DRM_FORMAT_XRGB8888.into(),
                                        modifier: 0,
                                    },
                                    ptr: unsafe { image.as_ptr() as _ },
                                    size,
                                    mouse: d
                                        .root_mouse_position()
                                        .map(|root_pos| {
                                            monitor.mouse_to_local(root_pos).map(|(x, y)| {
                                                MouseMeta {
                                                    x: (x as f32) / (image.width() as f32),
                                                    y: (y as f32) / (image.height() as f32),
                                                }
                                            })
                                        })
                                        .flatten(),
                                };
                                log::trace!("{}: captured frame", &monitor.name());

                                let frame = WlxFrame::MemPtr(memptr_frame);
                                match tx_frame.try_send(frame) {
                                    Ok(_) => (),
                                    Err(mpsc::TrySendError::Full(_)) => {
                                        log::debug!("{}: channel full", &monitor.name());
                                    }
                                    Err(mpsc::TrySendError::Disconnected(_)) => {
                                        log::warn!(
                                            "{}: capture thread channel closed (send)",
                                            &monitor.name(),
                                        );
                                        break;
                                    }
                                }
                            } else {
                                log::debug!("{}: XShmGetImage failed", &monitor.name());
                            }
                        }
                        Err(_) => {
                            log::warn!("{}: capture thread channel closed (recv)", monitor.name());
                            break;
                        }
                    }
                }
                log::warn!("{}: capture thread stopped", monitor.name());
            }
        });
    }
    fn is_ready(&self) -> bool {
        self.receiver.is_some()
    }
    fn supports_dmbuf(&self) -> bool {
        false
    }
    fn receive(&mut self) -> Option<WlxFrame> {
        if let Some(rx) = self.receiver.as_ref() {
            return rx.try_iter().last();
        }
        None
    }
    fn pause(&mut self) {}
    fn resume(&mut self) {
        self.receive(); // clear old frames
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
