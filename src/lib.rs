#![allow(dead_code)]
use std::sync::mpsc::Receiver;

use frame::WlxFrame;

pub mod frame;

#[cfg(feature = "wayland")]
pub mod wayland;

#[cfg(feature = "wlr")]
pub mod wlr_dmabuf;

#[cfg(feature = "pipewire")]
pub mod pipewire;

#[cfg(feature = "xshm")]
pub mod xshm;

pub trait WlxCapture {
    fn init(&mut self) -> Receiver<WlxFrame>;
    fn pause(&mut self);
    fn resume(&mut self);
    fn request_new_frame(&mut self);
}
