use std::sync::{Arc, Mutex};

use log::debug;

use smithay_client_toolkit::reexports::{
    protocols::xdg::xdg_output::zv1::client::{
        zxdg_output_manager_v1::ZxdgOutputManagerV1,
        zxdg_output_v1::{self, ZxdgOutputV1},
    },
    protocols_wlr::export_dmabuf::v1::client::zwlr_export_dmabuf_manager_v1::ZwlrExportDmabufManagerV1,
};

use wayland_client::{
    globals::{registry_queue_init, GlobalListContents},
    protocol::{
        wl_output::{self, Transform, WlOutput},
        wl_registry::WlRegistry,
    },
    Connection, Dispatch, EventQueue, Proxy, QueueHandle,
};

pub struct WlxOutput {
    pub wl_output: WlOutput,
    pub id: u32,
    pub name: Arc<str>,
    pub model: Arc<str>,
    pub size: (i32, i32),
    pub logical_pos: (i32, i32),
    pub logical_size: (i32, i32),
    pub transform: Transform,
    done: bool,
}

pub struct WlxClient {
    pub connection: Arc<Connection>,
    pub xdg_output_mgr: ZxdgOutputManagerV1,
    pub maybe_wlr_dmabuf_mgr: Option<ZwlrExportDmabufManagerV1>,
    pub outputs: Vec<WlxOutput>,
    pub queue: Arc<Mutex<EventQueue<Self>>>,
    pub queue_handle: QueueHandle<Self>,
}

impl WlxClient {
    pub fn new() -> Option<Self> {
        let connection = Connection::connect_to_env().ok()?;
        let (globals, queue) = registry_queue_init::<Self>(&connection).ok()?;
        let qh = queue.handle();

        let mut state = Self {
            connection: Arc::new(connection),
            xdg_output_mgr: globals
                .bind(&qh, 2..=3, ())
                .expect(ZxdgOutputManagerV1::interface().name),
            maybe_wlr_dmabuf_mgr: globals.bind(&qh, 1..=1, ()).ok(),
            outputs: vec![],
            queue: Arc::new(Mutex::new(queue)),
            queue_handle: qh.clone(),
        };

        for o in globals.contents().clone_list().iter() {
            if o.interface == WlOutput::interface().name {
                let wl_output: WlOutput = globals.registry().bind(o.name, o.version, &qh, o.name);

                state.xdg_output_mgr.get_xdg_output(&wl_output, &qh, o.name);

                let unknown: Arc<str> = "Unknown".into();

                let output = WlxOutput {
                    wl_output,
                    id: o.name,
                    name: unknown.clone(),
                    model: unknown.clone(),
                    size: (0, 0),
                    logical_pos: (0, 0),
                    logical_size: (0, 0),
                    transform: Transform::Normal,
                    done: false,
                };

                state.outputs.push(output);
            }
        }

        state.dispatch();

        Some(state)
    }

    /// Get the logical width and height of the desktop.
    pub fn get_desktop_extent(&self) -> (i32, i32) {
        let mut extent = (0, 0);
        for output in self.outputs.iter() {
            extent.0 = extent.0.max(output.logical_pos.0 + output.logical_size.0);
            extent.1 = extent.1.max(output.logical_pos.1 + output.logical_size.1);
        }
        extent
    }

    /// Dispatch pending events and block until finished.
    pub fn dispatch(&mut self) {
        if let Ok(mut queue_mut) = self.queue.clone().lock() {
            let _ = queue_mut.blocking_dispatch(self);
        }
    }
}

impl Dispatch<ZxdgOutputV1, u32> for WlxClient {
    fn event(
        state: &mut Self,
        _proxy: &ZxdgOutputV1,
        event: <ZxdgOutputV1 as Proxy>::Event,
        data: &u32,
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        match event {
            zxdg_output_v1::Event::Name { name } => {
                if let Some(output) = state.outputs.iter_mut().find(|o| o.id == *data) {
                    output.name = name.into();
                }
            }
            zxdg_output_v1::Event::LogicalPosition { x, y } => {
                if let Some(output) = state.outputs.iter_mut().find(|o| o.id == *data) {
                    output.logical_pos = (x, y);
                }
            }
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                if let Some(output) = state.outputs.iter_mut().find(|o| o.id == *data) {
                    output.logical_size = (width, height);
                }
            }
            zxdg_output_v1::Event::Done => {
                if let Some(output) = state.outputs.iter_mut().find(|o| o.id == *data) {
                    if output.logical_size.0 < 0 {
                        output.logical_pos.0 += output.logical_size.0;
                        output.logical_size.0 *= -1;
                    }
                    if output.logical_size.1 < 0 {
                        output.logical_pos.1 += output.logical_size.1;
                        output.logical_size.1 *= -1;
                    }
                    output.done = true;
                    debug!(
                        "Discovered WlOutput {}; Size: {:?}; Logical Size: {:?}; Pos: {:?}",
                        output.name,
                        output.size,
                        output.logical_size,
                        output.logical_pos
                    );
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WlOutput, u32> for WlxClient {
    fn event(
        state: &mut Self,
        _proxy: &WlOutput,
        event: <WlOutput as Proxy>::Event,
        data: &u32,
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        match event {
            wl_output::Event::Mode { width, height, .. } => {
                if let Some(output) = state.outputs.iter_mut().find(|o| o.id == *data) {
                    output.size = (width, height);
                }
            }
            wl_output::Event::Geometry {
                model, transform, ..
            } => {
                if let Some(output) = state.outputs.iter_mut().find(|o| o.id == *data) {
                    output.model = model.into();
                    output.transform = transform.into_result().unwrap_or(Transform::Normal);
                }
            }
            _ => {}
        }
    }
}

// Plumbing below

impl Dispatch<WlRegistry, ()> for WlxClient {
    fn event(
        _state: &mut Self,
        _proxy: &WlRegistry,
        _event: <WlRegistry as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZxdgOutputManagerV1, ()> for WlxClient {
    fn event(
        _state: &mut Self,
        _proxy: &ZxdgOutputManagerV1,
        _event: <ZxdgOutputManagerV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrExportDmabufManagerV1, ()> for WlxClient {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrExportDmabufManagerV1,
        _event: <ZwlrExportDmabufManagerV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for WlxClient {
    fn event(
        _state: &mut Self,
        _proxy: &WlRegistry,
        _event: <WlRegistry as Proxy>::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}
