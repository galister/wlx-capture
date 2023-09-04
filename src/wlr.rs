use std::{
    os::fd::IntoRawFd,
    sync::mpsc::{self, SendError, Sender, SyncSender},
    thread::JoinHandle,
};

use smithay_client_toolkit::reexports::protocols_wlr::export_dmabuf::v1::client::zwlr_export_dmabuf_frame_v1::{self, ZwlrExportDmabufFrameV1};
use wayland_client::{Connection, QueueHandle, Dispatch, Proxy};

use crate::{
    frame::{DmabufFrame, FramePlane, WlxFrame},
    wayland::WlxClient,
    WlxCapture,
};

use log::{warn,debug};

pub struct WlrDmabufCapture {
    output_idx: usize,
    wl: Option<Box<WlxClient>>,
    handle: Option<JoinHandle<Box<WlxClient>>>,
    sender: Option<Sender<WlxFrame>>,
}

impl WlrDmabufCapture {
    pub fn new(wl: WlxClient, output_id: u32) -> Option<Self> {
        let mut output_idx = None;
        for i in 0..wl.outputs.len() {
            if wl.outputs[i].id == output_id {
                output_idx = Some(i);
                break;
            }
        }
        output_idx.map(|output_idx| Self {
            output_idx,
            wl: Some(Box::new(wl)),
            handle: None,
            sender: None,
        })
    }
}

impl WlxCapture for WlrDmabufCapture {
    fn init(&mut self) -> std::sync::mpsc::Receiver<WlxFrame> {
        debug_assert!(self.wl.is_some());
        debug!("init wlr-dmabuf capture on output {}", self.wl.as_ref().unwrap().outputs[self.output_idx].name);

        let (tx, rx) = std::sync::mpsc::channel::<WlxFrame>();
        self.sender = Some(tx);
        rx
    }
    fn pause(&mut self) {}
    fn resume(&mut self) {}
    fn request_new_frame(&mut self) {
        if let Some(handle) = self.handle.take() {
            self.wl = Some(handle.join().unwrap());
        }

        let Some(wl) = self.wl.take() else {
            return;
        };

        self.handle = Some(std::thread::spawn({
            let sender = self
                .sender
                .clone()
                .expect("must call init once before request_new_frame");
            let output_idx = self.output_idx;
            move || request_dmabuf_frame(wl, output_idx, sender)
        }));
    }
}

/// Request a new DMA-Buf frame using the wlr-export-dmabuf protocol.
fn request_dmabuf_frame(
    client: Box<WlxClient>,
    output_idx: usize,
    sender: Sender<WlxFrame>,
) -> Box<WlxClient> {
    let Some(dmabuf_manager) = client.maybe_wlr_dmabuf_mgr.as_ref() else {
        return client;
    };

    let (tx, rx) = mpsc::sync_channel::<zwlr_export_dmabuf_frame_v1::Event>(1024);

    let _ = dmabuf_manager.capture_output(
        1,
        &client.outputs[output_idx].wl_output,
        &client.queue_handle,
        tx.clone(),
    );

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
            new_frame.format.fourcc = format;
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
            let _ = sender.send(WlxFrame::Dmabuf(frame));
        }
        zwlr_export_dmabuf_frame_v1::Event::Cancel { .. } => {
            warn!("DMA-Buf frame capture cancelled");
        }
        _ => {}
    });

    client
}

impl Dispatch<ZwlrExportDmabufFrameV1, SyncSender<zwlr_export_dmabuf_frame_v1::Event>>
    for WlxClient
{
    fn event(
        _state: &mut Self,
        proxy: &ZwlrExportDmabufFrameV1,
        event: <ZwlrExportDmabufFrameV1 as Proxy>::Event,
        data: &SyncSender<zwlr_export_dmabuf_frame_v1::Event>,
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
            Ok::<(), SendError<zwlr_export_dmabuf_frame_v1::Event>>(())
        });
    }
}
