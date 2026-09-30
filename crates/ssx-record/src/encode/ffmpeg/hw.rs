//! The few raw FFmpeg calls the safe wrapper does not cover: codec tags and the VA-API
//! hardware frame upload.
//!
//! Every other hardware encoder we use (NVENC, AMF, QSV, Media Foundation, VideoToolbox)
//! accepts ordinary NV12 system-memory frames, so only VA-API needs a hardware device and
//! a frame pool. Zero-copy import of GPU textures into these encoders is documentation
//! only (see the crate README).
//!
//! **Verification status:** [`HwUpload::vaapi`] compiles and is exercised by the runtime
//! prober, but the development box has no `/dev/dri` render node, so the upload itself has
//! only ever been run against the "no device" failure path.

#![allow(unsafe_code)] // FFI to libavutil/libavcodec; each block is minimal and commented.

use ffmpeg_next::{self as ff, ffi, frame};

/// Sets the FourCC written to the container for this stream (`hvc1` for HEVC in MP4).
pub(crate) fn set_codec_tag(video: &mut ff::encoder::video::Video, tag: [u8; 4]) {
    // SAFETY: `as_mut_ptr` returns the live AVCodecContext owned by `video`, and
    // `codec_tag` is a plain integer field that is only read when the encoder is opened.
    unsafe {
        (*video.as_mut_ptr()).codec_tag = u32::from_le_bytes(tag);
    }
}

/// A VA-API device plus the frame pool the encoder allocates surfaces from.
pub(crate) struct HwUpload {
    device: *mut ffi::AVBufferRef,
    frames: *mut ffi::AVBufferRef,
}

impl HwUpload {
    /// Creates the VA-API device (`$SSX_VAAPI_DEVICE`, else FFmpeg's default render node)
    /// and attaches an NV12 frame pool to the not-yet-opened encoder context.
    pub(crate) fn vaapi(
        video: &mut ff::encoder::video::Video,
        w: u32,
        h: u32,
    ) -> Result<Self, String> {
        let device_path =
            std::env::var("SSX_VAAPI_DEVICE").ok().and_then(|p| std::ffi::CString::new(p).ok());
        let mut device: *mut ffi::AVBufferRef = std::ptr::null_mut();
        // SAFETY: `device` is a valid out-pointer; the path, when given, is a NUL-terminated
        // string that outlives the call; the options dictionary is null (none).
        let ret = unsafe {
            ffi::av_hwdevice_ctx_create(
                &mut device,
                ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                device_path.as_ref().map_or(std::ptr::null(), |p| p.as_ptr()),
                std::ptr::null_mut(),
                0,
            )
        };
        if ret < 0 {
            return Err(format!("no VA-API device: {}", ff::Error::from(ret)));
        }
        // From here on `this` owns `device` and releases it on any early return.
        let mut this = Self { device, frames: std::ptr::null_mut() };
        // SAFETY: `device` is the valid device context created above.
        let frames = unsafe { ffi::av_hwframe_ctx_alloc(this.device) };
        if frames.is_null() {
            return Err("could not allocate the VA-API frame pool".into());
        }
        this.frames = frames;
        // SAFETY: `frames` is a valid AVBufferRef whose `data` is an AVHWFramesContext until
        // it is initialised; `av_hwframe_ctx_init` is the documented next step, and
        // `av_buffer_ref` gives the encoder its own reference to the pool.
        unsafe {
            let fc = (*frames).data.cast::<ffi::AVHWFramesContext>();
            (*fc).format = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI;
            (*fc).sw_format = ffi::AVPixelFormat::AV_PIX_FMT_NV12;
            (*fc).width = w as i32;
            (*fc).height = h as i32;
            (*fc).initial_pool_size = 20;
            let ret = ffi::av_hwframe_ctx_init(frames);
            if ret < 0 {
                return Err(format!("VA-API frame pool init failed: {}", ff::Error::from(ret)));
            }
            let r = ffi::av_buffer_ref(frames);
            if r.is_null() {
                return Err("out of memory".into());
            }
            (*video.as_mut_ptr()).hw_frames_ctx = r;
        }
        Ok(this)
    }

    /// Uploads a software NV12 frame into a VA-API surface, keeping its pts.
    pub(crate) fn upload(&mut self, sw: &frame::Video) -> Result<frame::Video, String> {
        let mut hw = frame::Video::empty();
        // SAFETY: `self.frames` is a valid initialised pool; `hw` and `sw` are valid AVFrames
        // that live for the whole block, and `av_hwframe_transfer_data` copies pixel data
        // between them without retaining either pointer.
        unsafe {
            let ret = ffi::av_hwframe_get_buffer(self.frames, hw.as_mut_ptr(), 0);
            if ret < 0 {
                return Err(format!("VA-API surface allocation failed: {}", ff::Error::from(ret)));
            }
            let ret = ffi::av_hwframe_transfer_data(hw.as_mut_ptr(), sw.as_ptr(), 0);
            if ret < 0 {
                return Err(format!("VA-API upload failed: {}", ff::Error::from(ret)));
            }
            (*hw.as_mut_ptr()).pts = (*sw.as_ptr()).pts;
        }
        Ok(hw)
    }
}

impl Drop for HwUpload {
    fn drop(&mut self) {
        // SAFETY: both pointers are either null or owned references created in `vaapi`;
        // `av_buffer_unref` accepts a pointer to a pointer and nulls it.
        unsafe {
            if !self.frames.is_null() {
                ffi::av_buffer_unref(&mut self.frames);
            }
            if !self.device.is_null() {
                ffi::av_buffer_unref(&mut self.device);
            }
        }
    }
}
