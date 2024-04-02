use std::{
    sync::{Arc, Mutex},
    time::Instant,
};

use idmap::IdMap;
use log::debug;

use smithay_client_toolkit::reexports::{
    protocols::xdg::xdg_output::zv1::client::{
        zxdg_output_manager_v1::ZxdgOutputManagerV1,
        zxdg_output_v1::{self, ZxdgOutputV1},
    },
    protocols_wlr::{
        export_dmabuf::v1::client::zwlr_export_dmabuf_manager_v1::ZwlrExportDmabufManagerV1,
        screencopy::v1::client::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
    },
};

pub use wayland_client;
use wayland_client::{
    globals::{registry_queue_init, GlobalList, GlobalListContents},
    protocol::{
        wl_output::{self, Transform, WlOutput},
        wl_registry::{self, WlRegistry},
        wl_seat::WlSeat,
        wl_shm::WlShm,
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
    pub dirty: bool,
    done: bool,
    updated: Instant,
}

pub struct WlxClient {
    pub connection: Arc<Connection>,
    pub xdg_output_mgr: ZxdgOutputManagerV1,
    pub maybe_wlr_dmabuf_mgr: Option<ZwlrExportDmabufManagerV1>,
    pub maybe_wlr_screencopy_mgr: Option<ZwlrScreencopyManagerV1>,
    pub wl_seat: WlSeat,
    pub wl_shm: WlShm,
    pub outputs: IdMap<u32, WlxOutput>,
    pub queue: Arc<Mutex<EventQueue<Self>>>,
    pub globals: GlobalList,
    pub queue_handle: QueueHandle<Self>,
    pub dirty: bool,
    default_output_name: Arc<str>,
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
            wl_seat: globals
                .bind(&qh, 4..=9, ())
                .expect(WlSeat::interface().name),
            wl_shm: globals.bind(&qh, 1..=1, ()).expect(WlShm::interface().name),
            maybe_wlr_dmabuf_mgr: globals.bind(&qh, 1..=1, ()).ok(),
            maybe_wlr_screencopy_mgr: globals.bind(&qh, 1..=1, ()).ok(),
            outputs: IdMap::new(),
            queue: Arc::new(Mutex::new(queue)),
            globals,
            queue_handle: qh,
            default_output_name: "Unknown".into(),
            dirty: false,
        };

        state.refresh_outputs();

        Some(state)
    }

    pub fn refresh_if_dirty(&mut self) -> bool {
        if self.dirty {
            let changed = self.refresh_outputs();
            self.dirty = false;
            changed
        } else {
            false
        }
    }

    pub fn refresh_outputs(&mut self) -> bool {
        let now = Instant::now();
        let mut changed = false;

        for o in self.globals.contents().clone_list().iter() {
            if o.interface == WlOutput::interface().name {
                let wl_output: WlOutput =
                    self.globals
                        .registry()
                        .bind(o.name, o.version, &self.queue_handle, o.name);

                if let Some(output) = self.outputs.get_mut(o.name) {
                    output.updated = now;
                } else {
                    self.xdg_output_mgr
                        .get_xdg_output(&wl_output, &self.queue_handle, o.name);
                    let output = WlxOutput {
                        wl_output,
                        id: o.name,
                        name: self.default_output_name.clone(),
                        model: self.default_output_name.clone(),
                        size: (0, 0),
                        logical_pos: (0, 0),
                        logical_size: (0, 0),
                        transform: Transform::Normal,
                        done: false,
                        dirty: false,
                        updated: now,
                    };

                    changed = true;
                    self.outputs.insert(o.name, output);
                    self.outputs.get_mut(o.name);
                }
            }
        }

        let old_len = self.outputs.len();
        self.outputs.retain(|_, o| o.updated == now);
        changed |= old_len != self.outputs.len();

        self.dispatch();
        changed
    }

    /// Get the logical width and height of the desktop.
    pub fn get_desktop_extent(&self) -> (i32, i32) {
        let mut extent = (0, 0);
        for output in self.outputs.values() {
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
        fn finalize_output(output: &mut WlxOutput) {
            if output.logical_size.0 < 0 {
                output.logical_pos.0 += output.logical_size.0;
                output.logical_size.0 *= -1;
            }
            if output.logical_size.1 < 0 {
                output.logical_pos.1 += output.logical_size.1;
                output.logical_size.1 *= -1;
            }
            output.dirty = false;
            output.done = true;
            debug!(
                "Discovered WlOutput {}; Size: {:?}; Logical Size: {:?}; Pos: {:?}",
                output.name, output.size, output.logical_size, output.logical_pos
            );
        }
        match event {
            zxdg_output_v1::Event::Name { name } => {
                if let Some(output) = state.outputs.get_mut(*data) {
                    output.name = name.into();
                }
            }
            zxdg_output_v1::Event::LogicalPosition { x, y } => {
                if let Some(output) = state.outputs.get_mut(*data) {
                    if output.done {
                        log::info!(
                            "{}: Logical pos changed {}x{} -> {}x{}",
                            output.name,
                            output.logical_pos.0,
                            output.logical_pos.1,
                            x,
                            y
                        );
                        output.dirty = true;
                        state.dirty = true;
                        return;
                    }

                    output.logical_pos = (x, y);
                    if output.logical_size != (0, 0) {
                        finalize_output(output);
                    }
                }
            }
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                if let Some(output) = state.outputs.get_mut(*data) {
                    if output.done {
                        log::info!(
                            "{}: Logical size changed {:?} -> {:?}",
                            output.name,
                            output.logical_size,
                            (width, height),
                        );
                        output.dirty = true;
                        state.dirty = true;
                        return;
                    }

                    output.logical_size = (width, height);
                    if output.logical_pos != (0, 0) {
                        finalize_output(output);
                    }
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
                if let Some(output) = state.outputs.get_mut(*data) {
                    if output.done && output.size != (width, height) {
                        log::info!(
                            "{}: Resolution changed {:?} -> {:?}",
                            output.name,
                            output.size,
                            (width, height)
                        );
                        output.dirty = true;
                        state.dirty = true;
                        return;
                    }
                    output.size = (width, height);
                }
            }
            wl_output::Event::Geometry {
                model, transform, ..
            } => {
                if let Some(output) = state.outputs.get_mut(*data) {
                    let transform = transform.into_result().unwrap_or(Transform::Normal);
                    if output.done && output.transform != transform {
                        log::info!(
                            "{}: Transform changed {:?} -> {:?}",
                            output.name,
                            output.transform,
                            transform
                        );
                        output.dirty = true;
                        state.dirty = true;
                        return;
                    }
                    output.model = model.into();
                    output.transform = transform;
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WlRegistry, ()> for WlxClient {
    fn event(
        state: &mut Self,
        _proxy: &WlRegistry,
        event: <WlRegistry as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name, interface, ..
            } => {
                if interface == WlOutput::interface().name {
                    log::info!("WlOutput {} added", name);
                    state.dirty = true;
                }
            }
            wl_registry::Event::GlobalRemove { name } => {
                if let Some(output) = state.outputs.get_mut(name) {
                    log::info!("WlOutput {} removed", name);
                    state.dirty = true;
                    output.dirty = true;
                }
            }
            _ => {}
        }
    }
}

// Plumbing below

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

impl Dispatch<ZwlrScreencopyManagerV1, ()> for WlxClient {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrScreencopyManagerV1,
        _event: <ZwlrScreencopyManagerV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSeat, ()> for WlxClient {
    fn event(
        _state: &mut Self,
        _proxy: &WlSeat,
        _event: <WlSeat as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlShm, ()> for WlxClient {
    fn event(
        _state: &mut Self,
        _proxy: &WlShm,
        _event: <WlShm as Proxy>::Event,
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
