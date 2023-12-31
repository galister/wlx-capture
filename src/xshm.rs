use log::{warn,error};
use std::{sync::{
    mpsc::Sender,
    Arc, Mutex,
}, time::Duration};

use once_cell::sync::Lazy;
use rxscreen::monitor::Monitor;

use crate::{frame::{WlxFrame, MemPtrFrame, FrameFormat, DRM_FORMAT_XRGB8888, MouseMeta}, WlxCapture};

static MUTEX: Lazy<Arc<Mutex<()>>> = Lazy::new(|| Arc::new(Mutex::new(())));

pub struct XshmScreen {
    name: Arc<str>,
    monitor: Monitor,
}

pub struct XshmCapture {
    screen: Arc<XshmScreen>,
    sender: Option<Sender<()>>,
}

impl XshmCapture {
    pub fn new(screen: XshmScreen) -> Option<Self> {
        Some(Self {
            screen: Arc::new(screen),
            sender: None,
        })
    }

    pub fn get_monitors() -> Vec<XshmScreen> {
        let d = rxscreen::Display::new(":0.0").unwrap();
        d.monitors()
            .into_iter()
            .enumerate()
            .map(|x| XshmScreen {
                name: format!("Scr {}", x.1.name()).into(),
                monitor: x.1,
            })
            .collect()
    }

}

impl WlxCapture for XshmCapture {
    fn init(&mut self) -> std::sync::mpsc::Receiver<WlxFrame> {
        let (tx_frame, rx_frame) = std::sync::mpsc::channel();
        let (tx_cmd, rx_cmd) = std::sync::mpsc::channel();
        self.sender = Some(tx_cmd);

        std::thread::spawn({
            let monitor = self.screen.monitor.clone();
            move || {
                let Ok(lock) = MUTEX.lock() else { 
                    error!("Scr {}: Failed to lock mutex", monitor.name());
                    return;
                };
                let Ok(d) = rxscreen::Display::new(":0.0") else {
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
                            let Ok(_lock) = MUTEX.lock() else { continue; };
                            if let Ok(image) = shm.capture() {

                                let frame = MemPtrFrame { 
                                    format: FrameFormat {
                                        width: image.width() as _,
                                        height: image.height() as _,
                                        fourcc: DRM_FORMAT_XRGB8888,
                                        modifier: 0,
                                    }, 
                                    ptr: unsafe { image.as_ptr() as _ },
                                };

                                if tx_frame.send(WlxFrame::MemPtr(frame)).is_err() {
                                    break;
                                }
                                
                                let Some(root_pos) = d.root_mouse_position() else { continue; };
                                let Some((x,y)) = monitor.mouse_to_local(root_pos) else { continue; };

                                let mouse = MouseMeta {
                                    x: x as _,
                                    y: y as _,
                                };
                                if tx_frame.send(WlxFrame::Mouse(mouse)).is_err() {
                                    break;
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

