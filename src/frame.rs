use libc::close;
use std::os::fd::RawFd;

pub type FourCC = u32;

pub const DRM_FORMAT_ARGB8888: FourCC = 0x34325241;
pub const DRM_FORMAT_ABGR8888: FourCC = 0x34324241;
pub const DRM_FORMAT_XRGB8888: FourCC = 0x34325258;
pub const DRM_FORMAT_XBGR8888: FourCC = 0x34324258;

#[rustfmt::skip]
const EGL_DMABUF_PLANE_ATTRS: [isize; 20] = [
//  FD     Offset Stride ModLo  ModHi
    0x3272,0x3273,0x3274,0x3443,0x3444,
    0x3275,0x3276,0x3277,0x3445,0x3446,
    0x3278,0x3279,0x327A,0x3447,0x3448,
    0x3440,0x3441,0x3442,0x3449,0x344A,
];

pub enum WlxFrame {
    Dmabuf(DmabufFrame),
    MemFd(MemFdFrame),
    MemPtr(MemPtrFrame),
    Mouse(MouseMeta),
}

#[derive(Debug, Clone, Copy, Default)]
pub struct FrameFormat {
    pub width: u32,
    pub height: u32,
    pub fourcc: FourCC,
    pub modifier: u64,
}

impl FrameFormat {
    pub fn get_mod_hi(&self) -> u32 {
        (self.modifier >> 32) as _
    }
    pub fn get_mod_lo(&self) -> u32 {
        (self.modifier & 0xFFFFFFFF) as _
    }
    pub fn set_mod(&mut self, mod_hi: u32, mod_low: u32) {
        self.modifier = ((mod_hi as u64) << 32) + mod_low as u64;
    }
}

#[derive(Clone, Copy, Default)]
pub struct FramePlane {
    pub fd: Option<RawFd>,
    pub offset: u32,
    pub stride: i32,
}

#[derive(Default)]
pub struct DrmFormat {
    pub fourcc: FourCC,
    pub modifiers: Vec<u64>,
}

#[derive(Default)]
pub struct DmabufFrame {
    pub format: FrameFormat,
    pub num_planes: usize,
    pub planes: [FramePlane; 4],
}

impl DmabufFrame {
    /// Get the attributes for creating an EGLImage.
    /// Pacics if fd is None; check using `is_valid` first.
    pub fn get_egl_image_attribs(&self) -> Vec<isize> {
        let mut vec: Vec<isize> = vec![
            0x3057, // WIDTH
            self.format.width as _,
            0x3056, // HEIGHT
            self.format.height as _,
            0x3271, // LINUX_DRM_FOURCC_EXT,
            self.format.fourcc as _,
        ];

        for i in 0..self.num_planes {
            let mut a = i * 5usize;
            vec.push(EGL_DMABUF_PLANE_ATTRS[a]);
            vec.push(self.planes[i].fd.unwrap() as _);
            a += 1;
            vec.push(EGL_DMABUF_PLANE_ATTRS[a]);
            vec.push(self.planes[i].offset as _);
            a += 1;
            vec.push(EGL_DMABUF_PLANE_ATTRS[a]);
            vec.push(self.planes[i].stride as _);
            a += 1;
            vec.push(EGL_DMABUF_PLANE_ATTRS[a]);
            vec.push(self.format.get_mod_lo() as _);
            a += 1;
            vec.push(EGL_DMABUF_PLANE_ATTRS[a]);
            vec.push(self.format.get_mod_hi() as _);
        }
        vec.push(0x3038); // NONE

        vec
    }

    /// Returns true if there's at least 1 valid fd.
    pub fn is_valid(&self) -> bool {
        self.planes[0].fd.is_some()
    }

    /// Close the file descriptors of all planes.
    /// Also called on drop.
    pub fn close(&mut self) {
        for i in 0..self.num_planes {
            if let Some(fd) = self.planes[i].fd {
                unsafe { close(fd) };
                self.planes[i].fd = None;
            }
        }
    }
}

impl Drop for DmabufFrame {
    fn drop(&mut self) {
        self.close();
    }
}

#[derive(Default)]
pub struct MemFdFrame {
    pub format: FrameFormat,
    pub plane: FramePlane,
}

#[derive(Default)]
pub struct MemPtrFrame {
    pub format: FrameFormat,
    pub ptr: usize,
}

#[derive(Default)]
pub struct MouseMeta {
    pub x: i32,
    pub y: i32,
}
