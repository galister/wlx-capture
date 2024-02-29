use std::sync::mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;

use ashpd::{
    desktop::screencast::{CursorMode, PersistMode, Screencast, SourceType},
    WindowIdentifier,
};

use log::debug;
use log::{error, info, warn};
use pipewire as pw;
use pw::properties;
use pw::spa::data::DataType;
use pw::spa::param::video::VideoFormat;
use pw::spa::param::video::VideoInfoRaw;
use pw::spa::param::ParamType;
use pw::spa::pod::serialize::GenError;
use pw::spa::pod::ChoiceValue;
use pw::spa::pod::Pod;
use pw::spa::pod::{Object, Property, PropertyFlags, Value};
use pw::spa::utils::Choice;
use pw::spa::utils::ChoiceEnum;
use pw::spa::utils::ChoiceFlags;
use pw::stream::{Stream, StreamFlags};
use pw::{Context, Error, MainLoop};

use crate::frame::DrmFormat;
use crate::frame::FourCC;
use crate::frame::FrameFormat;
use crate::frame::WlxFrame;
use crate::frame::DRM_FORMAT_ABGR8888;
use crate::frame::DRM_FORMAT_ARGB8888;
use crate::frame::DRM_FORMAT_XBGR8888;
use crate::frame::DRM_FORMAT_XRGB8888;
use crate::frame::{DmabufFrame, FramePlane, MemFdFrame, MemPtrFrame};
use crate::WlxCapture;

pub struct PipewireSelectScreenResult {
    pub node_id: u32,
    pub restore_token: Option<String>,
}

pub async fn pipewire_select_screen(
    token: Option<&str>,
    embed_mouse: bool,
    screens_only: bool,
    persist: bool,
) -> Result<PipewireSelectScreenResult, ashpd::Error> {
    let proxy = Screencast::new().await?;
    let session = proxy.create_session().await?;

    let cursor_mode = if embed_mouse {
        CursorMode::Embedded
    } else {
        CursorMode::Hidden
    };

    let source_type = if screens_only {
        SourceType::Monitor.into()
    } else {
        SourceType::Monitor | SourceType::Window | SourceType::Virtual
    };

    let persist_mode = if persist {
        PersistMode::ExplicitlyRevoked
    } else {
        PersistMode::DoNot
    };

    proxy
        .select_sources(
            &session,
            cursor_mode,
            source_type,
            false,
            token,
            persist_mode,
        )
        .await?;

    let response = proxy
        .start(&session, &WindowIdentifier::default())
        .await?
        .response()?;

    if let Some(stream) = response.streams().first() {
        return Ok(PipewireSelectScreenResult {
            node_id: stream.pipe_wire_node_id(),
            restore_token: response.restore_token().map(String::from),
        });
    }

    Err(ashpd::Error::NoResponse)
}

#[derive(Default)]
struct StreamData {
    format: Option<FrameFormat>,
    stream: Option<Stream>,
}

pub enum PwChangeRequest {
    Pause,
    Resume,
    Stop,
}

pub struct PipewireCapture {
    name: Arc<str>,
    tx_ctrl: Option<mpsc::SyncSender<PwChangeRequest>>,
    rx_frame: Option<mpsc::Receiver<WlxFrame>>,
    node_id: u32,
    fps: u32,
    handle: Option<JoinHandle<Result<(), Error>>>,
}

impl PipewireCapture {
    pub fn new(name: Arc<str>, node_id: u32, fps: u32) -> Self {
        PipewireCapture {
            name,
            tx_ctrl: None,
            rx_frame: None,
            node_id,
            fps,
            handle: None,
        }
    }
}

impl Drop for PipewireCapture {
    fn drop(&mut self) {
        if let Some(tx_ctrl) = &self.tx_ctrl {
            let _ = tx_ctrl.send(PwChangeRequest::Stop);
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl WlxCapture for PipewireCapture {
    fn init(&mut self, dmabuf_formats: &[DrmFormat]) {
        let (tx_frame, rx_frame) = mpsc::sync_channel(2);
        let (tx_ctrl, rx_ctrl) = mpsc::sync_channel(16);

        self.tx_ctrl = Some(tx_ctrl);
        self.rx_frame = Some(rx_frame);

        self.handle = Some(std::thread::spawn({
            let name = self.name.clone();
            let node_id = self.node_id;
            let fps = self.fps;
            let formats = dmabuf_formats.to_vec();

            move || main_loop(name, node_id, fps, formats, tx_frame, rx_ctrl)
        }));
    }
    fn is_ready(&self) -> bool {
        self.rx_frame.is_some()
    }
    fn supports_dmbuf(&self) -> bool {
        true
    }
    fn receive(&mut self) -> Option<WlxFrame> {
        if let Some(rx) = self.rx_frame.as_ref() {
            return rx.try_iter().last();
        }
        None
    }
    fn pause(&mut self) {
        if let Some(tx_ctrl) = &self.tx_ctrl {
            match tx_ctrl.try_send(PwChangeRequest::Pause) {
                Ok(_) => (),
                Err(mpsc::TrySendError::Full(_)) => {
                    warn!("{}: control channel full, cannot pause", &self.name);
                }
                Err(mpsc::TrySendError::Disconnected(_)) => {
                    warn!("{}: disconnected, stopping stream", &self.name);
                }
            }
        }
    }
    fn resume(&mut self) {
        if let Some(tx_ctrl) = &self.tx_ctrl {
            match tx_ctrl.try_send(PwChangeRequest::Resume) {
                Ok(_) => (),
                Err(mpsc::TrySendError::Full(_)) => {
                    error!("{}: control channel full, cannot resume", &self.name);
                }
                Err(mpsc::TrySendError::Disconnected(_)) => {
                    warn!("{}: disconnected, stopping stream", &self.name);
                }
            }
        }
        self.receive(); // clear old frames
    }
    fn request_new_frame(&mut self) {}
}

fn main_loop(
    name: Arc<str>,
    node_id: u32,
    fps: u32,
    dmabuf_formats: Vec<DrmFormat>,
    sender: mpsc::SyncSender<WlxFrame>,
    receiver: mpsc::Receiver<PwChangeRequest>,
) -> Result<(), Error> {
    let main_loop = MainLoop::new()?;
    let context = Context::new(&main_loop)?;
    let core = context.connect(None)?;

    let stream = Stream::new(
        &core,
        &name,
        properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )?;

    let _listener = stream
        .add_local_listener_with_user_data(FrameFormat::default())
        .state_changed({
            let name = name.clone();
            move |old, new| {
                info!("{}: stream state changed: {:?} -> {:?}", &name, old, new);
            }
        })
        .param_changed({
            let name = name.clone();
            move |stream, id, format, param| {
                let Some(param) = param else {
                    return;
                };
                if id != ParamType::Format.as_raw() {
                    return;
                }

                let mut info = VideoInfoRaw::default();
                info.parse(param)
                    .expect("Failed to parse param changed to VideoInfoRaw");

                format.width = info.size().width;
                format.height = info.size().height;
                format.fourcc = spa_to_fourcc(info.format());
                format.modifier = info.modifier();

                let kind = if format.modifier != 0 {
                    "DMA-buf"
                } else {
                    "SHM"
                };

                info!("{}: got {} video format:", &name, &kind);
                info!("  format: {} ({:?})", info.format().as_raw(), info.format());
                info!("  size: {}x{}", info.size().width, info.size().height);
                info!("  modifier: {}", info.modifier());
                if let Ok(params) = obj_to_bytes(get_buffer_params()) {
                    if let Err(e) = stream.update_params(&mut [params.as_ptr() as _]) {
                        error!("{}: failed to update params: {}", &name, e);
                    }
                }
            }
        })
        .process({
            let name = name.clone();
            move |stream, format| {
                let mut maybe_buffer = None;
                // discard all but the newest frame
                while let Some(buffer) = stream.dequeue_buffer() {
                    maybe_buffer = Some(buffer);
                }

                if let Some(mut buffer) = maybe_buffer {
                    let datas = buffer.datas_mut();
                    if datas.is_empty() {
                        debug!("{}: no data", &name);
                        return;
                    }

                    let planes: Vec<FramePlane> = datas
                        .iter()
                        .map(|p| FramePlane {
                            fd: Some(p.as_raw().fd as _),
                            offset: p.chunk().offset(),
                            stride: p.chunk().stride(),
                        })
                        .collect();

                    match datas[0].type_() {
                        DataType::DmaBuf => {
                            let mut dmabuf = DmabufFrame {
                                format: *format,
                                num_planes: planes.len(),
                                ..Default::default()
                            };
                            dmabuf.planes[..planes.len()].copy_from_slice(&planes[..planes.len()]);

                            let frame = WlxFrame::Dmabuf(dmabuf);
                            match sender.try_send(frame) {
                                Ok(_) => (),
                                Err(mpsc::TrySendError::Full(_)) => (),
                                Err(mpsc::TrySendError::Disconnected(_)) => {
                                    log::warn!("{}: disconnected, stopping stream", &name);
                                    let _ = stream.disconnect();
                                }
                            }
                        }
                        DataType::MemFd => {
                            let memfd = MemFdFrame {
                                format: *format,
                                plane: FramePlane {
                                    fd: Some(datas[0].as_raw().fd as _),
                                    offset: datas[0].chunk().offset(),
                                    stride: datas[0].chunk().stride(),
                                },
                            };

                            let frame = WlxFrame::MemFd(memfd);
                            match sender.try_send(frame) {
                                Ok(_) => (),
                                Err(mpsc::TrySendError::Full(_)) => (),
                                Err(mpsc::TrySendError::Disconnected(_)) => {
                                    log::warn!("{}: disconnected, stopping stream", &name);
                                    let _ = stream.disconnect();
                                }
                            }
                        }
                        DataType::MemPtr => {
                            let memptr = MemPtrFrame {
                                format: *format,
                                ptr: datas[0].as_raw().data as _,
                                size: datas[0].chunk().size() as _,
                                mouse: None,
                            };

                            let frame = WlxFrame::MemPtr(memptr);
                            match sender.try_send(frame) {
                                Ok(_) => (),
                                Err(mpsc::TrySendError::Full(_)) => (),
                                Err(mpsc::TrySendError::Disconnected(_)) => {
                                    log::warn!("{}: disconnected, stopping stream", &name);
                                    let _ = stream.disconnect();
                                }
                            }
                        }
                        _ => panic!("Unknown data type"),
                    }
                }
            }
        })
        .register()?;

    let mut format_params: Vec<Vec<u8>> = dmabuf_formats
        .iter()
        .filter_map(|f| obj_to_bytes(get_format_params(Some(f), fps)).ok())
        .collect();

    format_params.push(obj_to_bytes(get_format_params(None, fps)).unwrap()); // safe unwrap: known
                                                                             // good values

    let mut params: Vec<&Pod> = format_params
        .iter()
        .filter_map(|bytes| Pod::from_bytes(bytes))
        .collect();

    stream.connect(
        pw::spa::Direction::Input,
        Some(node_id),
        StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
        params.as_mut_slice(),
    )?;

    let trigger = main_loop.add_timer({
        let name = name.clone();
        let main_loop = main_loop.clone();
        move |_| {
            receiver.try_iter().for_each(|req| match req {
                PwChangeRequest::Pause => {
                    let _ = stream.set_active(false);
                }
                PwChangeRequest::Resume => {
                    let _ = stream.set_active(true);
                }
                PwChangeRequest::Stop => {
                    main_loop.quit();
                    info!("{}: stopping pipewire loop", &name);
                }
            })
        }
    });

    let interval = std::time::Duration::from_millis(250);
    trigger.update_timer(Some(interval), Some(interval));

    main_loop.run();
    info!("{}: pipewire loop exited", &name);
    Ok::<(), Error>(())
}

fn obj_to_bytes(obj: pw::spa::pod::Object) -> Result<Vec<u8>, GenError> {
    Ok(pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(obj),
    )?
    .0
    .into_inner())
}

fn get_buffer_params() -> Object {
    let data_types = (1 << pw::spa::data::DataType::MemFd.as_raw())
        | (1 << pw::spa::data::DataType::MemPtr.as_raw())
        | (1 << pw::spa::data::DataType::DmaBuf.as_raw());

    // TODO stop using libspa-sys when pipewire lib supports this
    let property = Property {
        key: libspa_sys::SPA_PARAM_BUFFERS_dataType,
        flags: PropertyFlags::empty(),
        value: Value::Int(data_types),
    };

    pw::spa::pod::object!(
        pw::spa::utils::SpaTypes::ObjectParamBuffers,
        pw::spa::param::ParamType::Buffers,
        property,
    )
}

fn get_format_params(fmt: Option<&DrmFormat>, fps: u32) -> Object {
    let mut obj = pw::spa::pod::object!(
        pw::spa::utils::SpaTypes::ObjectParamFormat,
        pw::spa::param::ParamType::EnumFormat,
        pw::spa::pod::property!(
            pw::spa::format::FormatProperties::MediaType,
            Id,
            pw::spa::format::MediaType::Video
        ),
        pw::spa::pod::property!(
            pw::spa::format::FormatProperties::MediaSubtype,
            Id,
            pw::spa::format::MediaSubtype::Raw
        ),
        pw::spa::pod::property!(
            pw::spa::format::FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            pw::spa::utils::Rectangle {
                width: 256,
                height: 256,
            },
            pw::spa::utils::Rectangle {
                width: 1,
                height: 1,
            },
            pw::spa::utils::Rectangle {
                width: 8192,
                height: 8192,
            }
        ),
        pw::spa::pod::property!(
            pw::spa::format::FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            pw::spa::utils::Fraction { num: fps, denom: 1 },
            pw::spa::utils::Fraction { num: 0, denom: 1 },
            pw::spa::utils::Fraction {
                num: 1000,
                denom: 1
            }
        ),
    );

    if let Some(fmt) = fmt {
        let spa_fmt = fourcc_to_spa(fmt.fourcc);

        let prop = pw::spa::pod::property!(
            pw::spa::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            spa_fmt,
            spa_fmt,
        );
        obj.properties.push(prop);

        // TODO rewrite when property macro supports Long
        let prop = Property {
            key: pw::spa::format::FormatProperties::VideoModifier.as_raw(),
            flags: PropertyFlags::MANDATORY | PropertyFlags::DONT_FIXATE,
            value: Value::Choice(ChoiceValue::Long(Choice(
                ChoiceFlags::from_bits_truncate(0),
                ChoiceEnum::Enum {
                    default: fmt.modifiers[0] as _,
                    alternatives: fmt.modifiers.iter().map(|m| *m as _).collect(),
                },
            ))),
        };
        obj.properties.push(prop);
    } else {
        let prop = pw::spa::pod::property!(
            pw::spa::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            pw::spa::param::video::VideoFormat::RGBA,
            pw::spa::param::video::VideoFormat::RGBA,
            pw::spa::param::video::VideoFormat::BGRA,
            pw::spa::param::video::VideoFormat::RGBx,
            pw::spa::param::video::VideoFormat::BGRx,
        );
        obj.properties.push(prop);
    }

    obj
}

fn fourcc_to_spa(fourcc: FourCC) -> VideoFormat {
    match fourcc.value {
        DRM_FORMAT_ARGB8888 => VideoFormat::BGRA,
        DRM_FORMAT_ABGR8888 => VideoFormat::RGBA,
        DRM_FORMAT_XRGB8888 => VideoFormat::BGRx,
        DRM_FORMAT_XBGR8888 => VideoFormat::RGBx,
        _ => panic!("Unsupported format"),
    }
}

#[allow(non_upper_case_globals)]
fn spa_to_fourcc(spa: VideoFormat) -> FourCC {
    match spa {
        VideoFormat::BGRA => DRM_FORMAT_ARGB8888.into(),
        VideoFormat::RGBA => DRM_FORMAT_ABGR8888.into(),
        VideoFormat::BGRx => DRM_FORMAT_XRGB8888.into(),
        VideoFormat::RGBx => DRM_FORMAT_XBGR8888.into(),
        _ => panic!("Unsupported format"),
    }
}
