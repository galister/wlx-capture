use std::sync::mpsc;
use std::sync::mpsc::Receiver;
use std::sync::mpsc::Sender;
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

pub async fn pipewire_select_screen(token: Option<&str>) -> Result<u32, ashpd::Error> {
    let proxy = Screencast::new().await?;
    let session = proxy.create_session().await?;

    proxy
        .select_sources(
            &session,
            CursorMode::Embedded,
            SourceType::Monitor | SourceType::Window,
            false,
            token,
            PersistMode::ExplicitlyRevoked,
        )
        .await?;

    let response = proxy
        .start(&session, &WindowIdentifier::default())
        .await?
        .response()?;

    if let Some(stream) = response.streams().first() {
        return Ok(stream.pipe_wire_node_id());
    }

    Err(ashpd::Error::NoResponse)
}

#[derive(Default)]
struct StreamData {
    format: Option<FrameFormat>,
    stream: Option<Stream>,
}

pub struct PipewireCapture {
    name: Arc<str>,
    node_id: u32,
    fps: u32,
    handle: Option<JoinHandle<Result<(), Error>>>,
}

impl PipewireCapture {
    pub fn new(name: Arc<str>, node_id: u32, fps: u32) -> Self {
        PipewireCapture {
            name,
            node_id,
            fps,
            handle: None,
        }
    }
}

impl WlxCapture for PipewireCapture {
    fn init(&mut self) -> Receiver<WlxFrame> {
        let (tx, rx) = mpsc::channel();

        self.handle = Some(std::thread::spawn({
            let name = self.name.clone();
            let node_id = self.node_id;
            let fps = self.fps;

            move || main_loop(name, node_id, fps, vec![], tx) // TODO dmabuf_formats
        }));

        rx
    }
    fn pause(&mut self) {}
    fn resume(&mut self) {}
    fn request_new_frame(&mut self) {}
}

fn main_loop(
    name: Arc<str>,
    node_id: u32,
    fps: u32,
    dmabuf_formats: Vec<DrmFormat>,
    sender: Sender<WlxFrame>,
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

                info!("{}: got video format:", &name);
                info!("  format: {} ({:?})", info.format().as_raw(), info.format());
                info!("  size: {}x{}", info.size().width, info.size().height);
                let params = obj_to_bytes(get_buffer_params());
                if let Err(e) = stream.update_params(&mut [params.as_ptr() as _]) {
                    error!("{}: failed to update params: {}", &name, e);
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

                            let _ = sender
                                .send(WlxFrame::Dmabuf(dmabuf))
                                .or_else(|_| stream.disconnect());
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
                            let _ = sender
                                .send(WlxFrame::MemFd(memfd))
                                .or_else(|_| stream.disconnect());
                        }
                        DataType::MemPtr => {
                            let memptr = MemPtrFrame {
                                format: *format,
                                ptr: datas[0].as_raw().data as _,
                            };
                            let _ = sender
                                .send(WlxFrame::MemPtr(memptr))
                                .or_else(|_| stream.disconnect());
                        }
                        _ => panic!("Unknown data type"),
                    }
                }
            }
        })
        .register()?;

    let mut format_params: Vec<Vec<u8>> = dmabuf_formats
        .iter()
        .map(|f| obj_to_bytes(get_format_params(Some(f), fps)))
        .collect();
    format_params.push(obj_to_bytes(get_format_params(None, fps)));

    let mut params: Vec<&Pod> = format_params
        .iter()
        .map(|bytes| Pod::from_bytes(bytes).unwrap())
        .collect();

    stream.connect(
        pw::spa::Direction::Input,
        Some(node_id),
        StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
        params.as_mut_slice(),
    )?;

    main_loop.run();
    warn!("{}: pipewire loop exited", &name);
    Ok::<(), Error>(())
}

fn obj_to_bytes(obj: pw::spa::pod::Object) -> Vec<u8> {
    pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(obj),
    )
    .unwrap()
    .0
    .into_inner()
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
    match fourcc {
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
        VideoFormat::BGRA => DRM_FORMAT_ARGB8888,
        VideoFormat::RGBA => DRM_FORMAT_ABGR8888,
        VideoFormat::BGRx => DRM_FORMAT_XRGB8888,
        VideoFormat::RGBx => DRM_FORMAT_XBGR8888,
        _ => panic!("Unsupported format"),
    }
}
