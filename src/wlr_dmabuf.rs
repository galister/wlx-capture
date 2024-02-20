use std::{os::fd::IntoRawFd, sync::mpsc, thread::JoinHandle};

use smithay_client_toolkit::reexports::protocols_wlr::export_dmabuf::v1::client::zwlr_export_dmabuf_frame_v1::{self, ZwlrExportDmabufFrameV1};
use wayland_client::{Connection, QueueHandle, Dispatch, Proxy};

use crate::{
    frame::{DmabufFrame, DrmFormat, FramePlane, WlxFrame},
    wayland::WlxClient,
    WlxCapture,
};

use log::{debug, warn};

pub struct WlrDmabufCapture {
    output_id: u32,
    wl: Option<Box<WlxClient>>,
    handle: Option<JoinHandle<Box<WlxClient>>>,
    sender: Option<mpsc::SyncSender<WlxFrame>>,
    receiver: Option<mpsc::Receiver<WlxFrame>>,
}

impl WlrDmabufCapture {
    pub fn new(wl: WlxClient, output_id: u32) -> Self {
        Self {
            output_id,
            wl: Some(Box::new(wl)),
            handle: None,
            sender: None,
            receiver: None,
        }
    }
}

impl WlxCapture for WlrDmabufCapture {
    fn init(&mut self, _: &[DrmFormat]) {
        debug_assert!(self.wl.is_some());

        let (tx, rx) = std::sync::mpsc::sync_channel::<WlxFrame>(2);
        self.sender = Some(tx);
        self.receiver = Some(rx);
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
        if let Some(handle) = self.handle.take() {
            if handle.is_finished() {
                self.wl = Some(handle.join().unwrap()); // safe to unwrap because we checked is_finished
            } else {
                self.handle = Some(handle);
                return;
            }
        }

        let Some(wl) = self.wl.take() else {
            return;
        };

        self.handle = Some(std::thread::spawn({
            let sender = self
                .sender
                .clone()
                .expect("must call init once before request_new_frame");
            let output_id = self.output_id;
            move || request_dmabuf_frame(wl, output_id, sender)
        }));
    }
}

/// Request a new DMA-Buf frame using the wlr-export-dmabuf protocol.
fn request_dmabuf_frame(
    client: Box<WlxClient>,
    output_id: u32,
    sender: mpsc::SyncSender<WlxFrame>,
) -> Box<WlxClient> {
    let Some(dmabuf_manager) = client.maybe_wlr_dmabuf_mgr.as_ref() else {
        return client;
    };

    let Some(output) = client.outputs.get(output_id) else {
        return client;
    };

    let (tx, rx) = mpsc::sync_channel::<zwlr_export_dmabuf_frame_v1::Event>(16);
    let name = output.name.clone();

    let _ = dmabuf_manager.capture_output(1, &output.wl_output, &client.queue_handle, tx.clone());

    let mut client = client;
    client.dispatch();

    let mut frame = None;

    rx.try_iter().for_each(|event| match event {
        zwlr_export_dmabuf_frame_v1::Event::Frame {
            width,
            height,
            format,
            mod_high,
            mod_low,
            num_objects,
            ..
        } => {
            let mut new_frame = DmabufFrame::default();
            new_frame.format.width = width;
            new_frame.format.height = height;
            new_frame.format.fourcc.value = format;
            new_frame.format.set_mod(mod_high, mod_low);
            new_frame.num_planes = num_objects as _;
            frame = Some(new_frame);
        }
        zwlr_export_dmabuf_frame_v1::Event::Object {
            index,
            fd,
            offset,
            stride,
            ..
        } => {
            let Some(ref mut frame) = frame else {
                return;
            };
            frame.planes[index as usize] = FramePlane {
                fd: Some(fd.into_raw_fd()),
                offset,
                stride: stride as _,
            };
        }
        zwlr_export_dmabuf_frame_v1::Event::Ready { .. } => {
            let Some(frame) = frame.take() else {
                return;
            };
            debug!("DMA-Buf frame captured");
            let frame = WlxFrame::Dmabuf(frame);
            match sender.try_send(frame) {
                Ok(_) => (),
                Err(mpsc::TrySendError::Full(_)) => (),
                Err(mpsc::TrySendError::Disconnected(_)) => {
                    log::warn!("{}: disconnected", &name);
                }
            }
        }
        zwlr_export_dmabuf_frame_v1::Event::Cancel { .. } => {
            warn!("DMA-Buf frame capture cancelled");
        }
        _ => {}
    });

    client
}

impl Dispatch<ZwlrExportDmabufFrameV1, mpsc::SyncSender<zwlr_export_dmabuf_frame_v1::Event>>
    for WlxClient
{
    fn event(
        _state: &mut Self,
        proxy: &ZwlrExportDmabufFrameV1,
        event: <ZwlrExportDmabufFrameV1 as Proxy>::Event,
        data: &mpsc::SyncSender<zwlr_export_dmabuf_frame_v1::Event>,
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_export_dmabuf_frame_v1::Event::Ready { .. }
            | zwlr_export_dmabuf_frame_v1::Event::Cancel { .. } => {
                proxy.destroy();
            }
            _ => {}
        }

        let _ = data.send(event).or_else(|err| {
            warn!("Failed to send DMA-Buf frame event: {}", err);
            Ok::<(), mpsc::SendError<zwlr_export_dmabuf_frame_v1::Event>>(())
        });
    }
}
