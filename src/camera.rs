//! Photographing a window, from inside the process that owns it.
//!
/// Asking the window server for the rectangle this window occupies. A
/// process may photograph its own windows without the screen-recording
/// permission a capture of the whole display would need, which is what
/// makes the screenshots in the design document possible at all.
#[cfg(target_os = "macos")]
mod mac {
    use std::ffi::c_void;

    use anyhow::{anyhow, Result};

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CGPoint {
        x: f64,
        y: f64,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CGSize {
        width: f64,
        height: f64,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CGRect {
        origin: CGPoint,
        size: CGSize,
    }

    /// On screen, excluding the desktop's own furniture.
    const ON_SCREEN_ONLY: u32 = 1;
    const EXCLUDE_DESKTOP: u32 = 16;
    /// Eight bits a channel, R G B A in that order in memory.
    const ALPHA_PREMULTIPLIED_LAST: u32 = 1;
    const BYTE_ORDER_32_BIG: u32 = 4 << 12;

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGWindowListCreateImage(
            rect: CGRect,
            option: u32,
            window: u32,
            image_option: u32,
        ) -> *mut c_void;
        fn CGImageGetWidth(image: *mut c_void) -> usize;
        fn CGImageGetHeight(image: *mut c_void) -> usize;
        fn CGImageRelease(image: *mut c_void);
        fn CGColorSpaceCreateDeviceRGB() -> *mut c_void;
        fn CGColorSpaceRelease(space: *mut c_void);
        fn CGBitmapContextCreate(
            data: *mut c_void,
            width: usize,
            height: usize,
            bits_per_component: usize,
            bytes_per_row: usize,
            space: *mut c_void,
            info: u32,
        ) -> *mut c_void;
        fn CGContextDrawImage(context: *mut c_void, rect: CGRect, image: *mut c_void);
        fn CGContextRelease(context: *mut c_void);
    }

    pub(crate) fn grab(x: f64, y: f64, width: f64, height: f64) -> Result<image::RgbaImage> {
        let rect = CGRect {
            origin: CGPoint { x, y },
            size: CGSize { width, height },
        };
        unsafe {
            let image =
                CGWindowListCreateImage(rect, ON_SCREEN_ONLY | EXCLUDE_DESKTOP, 0, 0);
            if image.is_null() {
                return Err(anyhow!("the window server handed back no image"));
            }
            let width = CGImageGetWidth(image);
            let height = CGImageGetHeight(image);
            let mut pixels = vec![0u8; width * height * 4];
            let space = CGColorSpaceCreateDeviceRGB();
            let context = CGBitmapContextCreate(
                pixels.as_mut_ptr().cast(),
                width,
                height,
                8,
                width * 4,
                space,
                ALPHA_PREMULTIPLIED_LAST | BYTE_ORDER_32_BIG,
            );
            if context.is_null() {
                CGColorSpaceRelease(space);
                CGImageRelease(image);
                return Err(anyhow!("no bitmap to draw the frame into"));
            }
            CGContextDrawImage(
                context,
                CGRect {
                    origin: CGPoint { x: 0., y: 0. },
                    size: CGSize {
                        width: width as f64,
                        height: height as f64,
                    },
                },
                image,
            );
            CGContextRelease(context);
            CGColorSpaceRelease(space);
            CGImageRelease(image);
            image::RgbaImage::from_raw(width as u32, height as u32, pixels)
                .ok_or_else(|| anyhow!("the frame did not fit its own dimensions"))
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod mac {
    use anyhow::{anyhow, Result};

    pub(crate) fn grab(_x: f64, _y: f64, _w: f64, _h: f64) -> Result<image::RgbaImage> {
        Err(anyhow!("photographing the window is macOS only"))
    }
}


pub(crate) use mac::grab;
