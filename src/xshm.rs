use log::{error, warn};
use std::{
    env,
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
}

impl XshmCapture {
    pub fn new(screen: Arc<XshmScreen>) -> Self {
        Self {
            screen,
            sender: None,
        }
    }

    pub fn get_monitors() -> Vec<Arc<XshmScreen>> {
        let display = env::var("DISPLAY").expect("DISPLAY not set");
        let d = rxscreen::Display::new(display).unwrap();
        d.monitors()
            .into_iter()
            .enumerate()
            .map(|x| {
                Arc::new(XshmScreen {
                    name: format!("Scr {}", x.1.name()).into(),
                    monitor: x.1,
                })
            })
            .collect()
    }
}

impl WlxCapture for XshmCapture {
    fn init(&mut self, _: &[DrmFormat]) -> std::sync::mpsc::Receiver<WlxFrame> {
        let (tx_frame, rx_frame) = std::sync::mpsc::sync_channel(4);
        let (tx_cmd, rx_cmd) = std::sync::mpsc::sync_channel(2);
        self.sender = Some(tx_cmd);

        std::thread::spawn({
            let monitor = self.screen.monitor.clone();
            move || {
                let Ok(lock) = MUTEX.lock() else {
                    error!("Scr {}: Failed to lock mutex", monitor.name());
                    return;
                };
                let display = env::var("DISPLAY").expect("DISPLAY not set");
                let Ok(d) = rxscreen::Display::new(display) else {
                    error!("Scr {}: Failed to open display", monitor.name());
                    return;
                };
                let Ok(shm) = d.shm().monitor(&monitor).build() else {
                    error!("Scr {}: Failed to create shm", monitor.name());
                    return;
                };
                drop(lock);
                let sleep_duration = Duration::from_millis(1);

                loop {
                    match rx_cmd.try_iter().last() {
                        Some(_) => {
                            let Ok(_lock) = MUTEX.lock() else {
                                continue;
                            };
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
                                };

                                let Some(root_pos) = d.root_mouse_position() else {
                                    continue;
                                };
                                let Some((x, y)) = monitor.mouse_to_local(root_pos) else {
                                    continue;
                                };

                                let mouse = MouseMeta {
                                    x: x as _,
                                    y: y as _,
                                };
                                let frame = WlxFrame::Mouse(mouse);

                                match tx_frame.try_send(frame) {
                                    Ok(_) => (),
                                    Err(mpsc::TrySendError::Full(_)) => (),
                                    Err(mpsc::TrySendError::Disconnected(_)) => {
                                        log::warn!(
                                            "{}: disconnected, stopping capture thread",
                                            &monitor.name(),
                                        );
                                        break;
                                    }
                                }

                                let frame = WlxFrame::MemPtr(memptr_frame);
                                match tx_frame.try_send(frame) {
                                    Ok(_) => (),
                                    Err(mpsc::TrySendError::Full(_)) => (),
                                    Err(mpsc::TrySendError::Disconnected(_)) => {
                                        log::warn!(
                                            "{}: disconnected, stopping capture thread",
                                            &monitor.name(),
                                        );
                                        break;
                                    }
                                }
                            }
                        }
                        None => {
                            std::thread::sleep(sleep_duration);
                        }
                    }
                }
                warn!("Scr {}: Capture thread stopped", monitor.name());
            }
        });
        rx_frame
    }
    fn pause(&mut self) {}
    fn resume(&mut self) {}
    fn request_new_frame(&mut self) {
        if let Some(sender) = &self.sender {
            let _ = sender.send(());
        }
    }
}
