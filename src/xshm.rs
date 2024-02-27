use log::{error, trace, warn};
use std::{
    env,
    error::Error,
    sync::{mpsc, Arc, Mutex},
    time::Duration,
};

use once_cell::sync::Lazy;
use rxscreen::monitor::Monitor;

use crate::{
    frame::{DrmFormat, FrameFormat, MemPtrFrame, MouseMeta, WlxFrame, DRM_FORMAT_XRGB8888},
    WlxCapture,
};

static MUTEX: Lazy<Arc<Mutex<()>>> = Lazy::new(|| Arc::new(Mutex::new(())));

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
                let Ok(lock) = MUTEX.lock() else {
                    error!("{}: Failed to lock mutex", monitor.name());
                    return;
                };
                let display = env::var("DISPLAY").expect("DISPLAY not set");
                let Ok(d) = rxscreen::Display::new(display) else {
                    error!("{}: Failed to open display", monitor.name());
                    return;
                };
                let Ok(shm) = d.shm().monitor(&monitor).build() else {
                    error!("{}: Failed to create shm", monitor.name());
                    return;
                };
                drop(lock);
                let sleep_duration = Duration::from_millis(1);

                loop {
                    match rx_cmd.try_iter().last() {
                        Some(_) => {
                            let Ok(lock) = MUTEX.lock() else {
                                continue;
                            };
                            if let Ok(image) = shm.capture() {
                                let size = unsafe { image.as_bytes().len() };
                                let mut memptr_frame = MemPtrFrame {
                                    format: FrameFormat {
                                        width: image.width() as _,
                                        height: image.height() as _,
                                        fourcc: DRM_FORMAT_XRGB8888.into(),
                                        modifier: 0,
                                    },
                                    ptr: unsafe { image.as_ptr() as _ },
                                    size,
                                    mouse: None,
                                };
                                log::trace!("{}: captured frame", &monitor.name());

                                let Some(root_pos) = d.root_mouse_position() else {
                                    continue;
                                };
                                let Some((x, y)) = monitor.mouse_to_local(root_pos) else {
                                    continue;
                                };

                                memptr_frame.mouse = Some(MouseMeta {
                                    x: (x as f32) / (image.width() as f32),
                                    y: (y as f32) / (image.height() as f32),
                                });

                                let frame = WlxFrame::MemPtr(memptr_frame);
                                match tx_frame.try_send(frame) {
                                    Ok(_) => (),
                                    Err(mpsc::TrySendError::Full(_)) => {
                                        trace!("{}: channel full", &monitor.name());
                                    }
                                    Err(mpsc::TrySendError::Disconnected(_)) => {
                                        log::warn!(
                                            "{}: receiver disconnected, stopping capture thread",
                                            &monitor.name(),
                                        );
                                        break;
                                    }
                                }
                            } else {
                                log::debug!("{}: XShmGetImage failed", &monitor.name());
                            }
                            drop(lock);
                        }
                        None => {
                            std::thread::sleep(sleep_duration);
                        }
                    }
                }
                warn!("{}: Capture thread stopped", monitor.name());
            }
        });
    }
    fn ready(&self) -> bool {
        self.receiver.is_some()
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
    }
    fn request_new_frame(&mut self) {
        if let Some(sender) = &self.sender {
            let _ = sender.send(());
        }
    }
}
