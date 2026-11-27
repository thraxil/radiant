//! Small safe wrappers over the parts of libwebp that radiant uses.
//!
//! We call libwebp directly, rather than through image's pure-Rust WebP decoder or the `webp`
//! crate, because of two bugs:
//! * image-webp composites some animations wrongly. A fully transparent frame kept showing
//!   the frame before it.
//! * The `webp` crate's animation encoder ends every animation with a timestamp of 0, so
//!   libwebp has to guess the last frame's duration.
//!
//! libwebp is the reference implementation, and we already link it for encoding.

use std::ffi::{CStr, c_int};
use std::marker::PhantomData;
use std::mem::MaybeUninit;
use std::ptr::{self, NonNull};

use image::RgbaImage;
use libwebp_sys as sys;

/// Encodes a still image. `channels` is 3 (RGB) or 4 (RGBA).
pub(super) fn encode_still(
    pixels: &[u8],
    width: u32,
    height: u32,
    channels: u32,
    quality: f32,
) -> Result<Vec<u8>, String> {
    assert!(channels == 3 || channels == 4);
    let expected = width as usize * height as usize * channels as usize;
    if pixels.len() != expected {
        return Err(format!(
            "expected {expected} bytes of pixels, got {}",
            pixels.len()
        ));
    }
    let stride = c_int::try_from(width * channels).map_err(|_| "image too wide".to_owned())?;
    let mut out: *mut u8 = ptr::null_mut();
    // SAFETY: `pixels` holds `height` rows of `stride` bytes (checked above). On success libwebp
    // allocates `out`, which we copy and then free.
    let len = unsafe {
        if channels == 4 {
            sys::WebPEncodeRGBA(
                pixels.as_ptr(),
                width as c_int,
                height as c_int,
                stride,
                quality,
                &mut out,
            )
        } else {
            sys::WebPEncodeRGB(
                pixels.as_ptr(),
                width as c_int,
                height as c_int,
                stride,
                quality,
                &mut out,
            )
        }
    };
    if len == 0 || out.is_null() {
        return Err("libwebp failed to encode image".into());
    }
    // SAFETY: libwebp returned a buffer of `len` bytes at `out`.
    let bytes = unsafe { std::slice::from_raw_parts(out, len) }.to_vec();
    // SAFETY: `out` was allocated by libwebp and is not used after this.
    unsafe { sys::WebPFree(out.cast()) };
    Ok(bytes)
}

/// Streaming decoder for animated WebP. Frames come out composited onto the full canvas, as
/// non-premultiplied RGBA.
pub(super) struct AnimDecoder<'a> {
    dec: NonNull<sys::WebPAnimDecoder>,
    info: sys::WebPAnimInfo,
    prev_timestamp: c_int,
    /// libwebp keeps a pointer to the input for the decoder's whole life.
    _data: PhantomData<&'a [u8]>,
}

impl<'a> AnimDecoder<'a> {
    pub fn new(bytes: &'a [u8]) -> Result<Self, String> {
        let mut opts = MaybeUninit::<sys::WebPAnimDecoderOptions>::uninit();
        // SAFETY: initialises the options struct in place. We only read it on success.
        if unsafe { sys::WebPAnimDecoderOptionsInit(opts.as_mut_ptr()) } == 0 {
            return Err("libwebp version mismatch".into());
        }
        // SAFETY: initialised by the call above.
        let mut opts = unsafe { opts.assume_init() };
        opts.color_mode = sys::WEBP_CSP_MODE::MODE_RGBA;
        opts.use_threads = 0;
        let data = sys::WebPData {
            bytes: bytes.as_ptr(),
            size: bytes.len(),
        };
        // SAFETY: `data` points at `bytes`, which outlives the decoder (the 'a lifetime).
        // libwebp copies the WebPData struct itself.
        let dec = unsafe { sys::WebPAnimDecoderNew(&data, &opts) };
        let dec = NonNull::new(dec).ok_or("not a valid animated WebP")?;
        let mut info = sys::WebPAnimInfo::default();
        // SAFETY: `dec` is a live decoder.
        if unsafe { sys::WebPAnimDecoderGetInfo(dec.as_ptr(), &mut info) } == 0 {
            // SAFETY: `dec` is live and not used again.
            unsafe { sys::WebPAnimDecoderDelete(dec.as_ptr()) };
            return Err("could not read animated WebP info".into());
        }
        Ok(Self {
            dec,
            info,
            prev_timestamp: 0,
            _data: PhantomData,
        })
    }

    pub fn canvas(&self) -> (u32, u32) {
        (self.info.canvas_width, self.info.canvas_height)
    }

    pub fn frame_count(&self) -> u32 {
        self.info.frame_count
    }

    /// 0 means loop forever.
    pub fn loop_count(&self) -> u32 {
        self.info.loop_count
    }

    /// The next composited frame and its duration in milliseconds.
    pub fn next_frame(&mut self) -> Result<Option<(RgbaImage, u32)>, String> {
        // SAFETY: `self.dec` is live for as long as `self` exists.
        if unsafe { sys::WebPAnimDecoderHasMoreFrames(self.dec.as_ptr()) } == 0 {
            return Ok(None);
        }
        let mut buf: *mut u8 = ptr::null_mut();
        let mut timestamp: c_int = 0;
        // SAFETY: as above. On success `buf` points at a canvas-sized RGBA buffer owned by the
        // decoder, valid until the next call.
        if unsafe { sys::WebPAnimDecoderGetNext(self.dec.as_ptr(), &mut buf, &mut timestamp) } == 0
            || buf.is_null()
        {
            return Err("corrupt animated WebP frame".into());
        }
        let (w, h) = self.canvas();
        let len = (w as usize)
            .checked_mul(h as usize)
            .and_then(|n| n.checked_mul(4))
            .ok_or("WebP canvas too large")?;
        // SAFETY: libwebp guarantees `buf` holds width * height * 4 bytes in MODE_RGBA.
        let pixels = unsafe { std::slice::from_raw_parts(buf, len) }.to_vec();
        // Timestamps mark the *end* of each frame.
        let delay = (timestamp - self.prev_timestamp).max(0) as u32;
        self.prev_timestamp = timestamp;
        let img = RgbaImage::from_raw(w, h, pixels).ok_or("bad WebP frame buffer")?;
        Ok(Some((img, delay)))
    }
}

impl Drop for AnimDecoder<'_> {
    fn drop(&mut self) {
        // SAFETY: `self.dec` came from WebPAnimDecoderNew and is deleted exactly once.
        unsafe { sys::WebPAnimDecoderDelete(self.dec.as_ptr()) };
    }
}

struct AnimEncoder(NonNull<sys::WebPAnimEncoder>);

impl AnimEncoder {
    fn error(&self) -> String {
        // SAFETY: the encoder is live. The returned string is owned by it.
        let msg = unsafe { sys::WebPAnimEncoderGetError(self.0.as_ptr()) };
        if msg.is_null() {
            "unknown libwebp error".into()
        } else {
            // SAFETY: libwebp returns a NUL-terminated string.
            unsafe { CStr::from_ptr(msg) }
                .to_string_lossy()
                .into_owned()
        }
    }
}

impl Drop for AnimEncoder {
    fn drop(&mut self) {
        // SAFETY: created by WebPAnimEncoderNewInternal, deleted exactly once.
        unsafe { sys::WebPAnimEncoderDelete(self.0.as_ptr()) };
    }
}

struct Picture(sys::WebPPicture);

impl Drop for Picture {
    fn drop(&mut self) {
        // SAFETY: the picture was initialised. Freeing an un-imported picture is a no-op.
        unsafe { sys::WebPPictureFree(&mut self.0) };
    }
}

/// Encodes an animation. Each frame is `width`×`height` RGBA with a duration in ms.
/// `loop_count` 0 means loop forever.
pub(super) fn encode_animation(
    width: u32,
    height: u32,
    frames: &[(RgbaImage, u32)],
    loop_count: u32,
    quality: f32,
) -> Result<Vec<u8>, String> {
    let (w, h) = (
        c_int::try_from(width).map_err(|_| "too wide")?,
        c_int::try_from(height).map_err(|_| "too tall")?,
    );

    let mut opts = MaybeUninit::<sys::WebPAnimEncoderOptions>::uninit();
    // SAFETY: initialises the options in place. Read only on success.
    if unsafe {
        sys::WebPAnimEncoderOptionsInitInternal(opts.as_mut_ptr(), sys::WebPGetMuxABIVersion())
    } == 0
    {
        return Err("libwebp version mismatch".into());
    }
    // SAFETY: initialised above.
    let mut opts = unsafe { opts.assume_init() };
    opts.anim_params.loop_count = c_int::try_from(loop_count).unwrap_or(c_int::MAX);

    // SAFETY: valid options. The result is checked for null and owned by the guard.
    let enc = unsafe { sys::WebPAnimEncoderNewInternal(w, h, &opts, sys::WebPGetMuxABIVersion()) };
    let enc = AnimEncoder(NonNull::new(enc).ok_or("could not create WebP animation encoder")?);

    let mut config = sys::WebPConfig::new().map_err(|()| "libwebp config init failed")?;
    config.lossless = 0;
    config.quality = quality;
    config.thread_level = 0;
    // SAFETY: `config` is initialised.
    if unsafe { sys::WebPValidateConfig(&config) } == 0 {
        return Err("invalid WebP config".into());
    }

    let mut timestamp: c_int = 0;
    for (img, delay) in frames {
        if img.dimensions() != (width, height) {
            return Err("frame size does not match canvas".into());
        }
        let mut pic = MaybeUninit::<sys::WebPPicture>::uninit();
        // SAFETY: initialises the picture in place. Read only on success.
        if unsafe {
            sys::WebPPictureInitInternal(pic.as_mut_ptr(), sys::WEBP_ENCODER_ABI_VERSION as c_int)
        } == 0
        {
            return Err("libwebp version mismatch".into());
        }
        // SAFETY: initialised above. The guard frees whatever ImportRGBA allocates.
        let mut pic = Picture(unsafe { pic.assume_init() });
        pic.0.use_argb = 1;
        pic.0.width = w;
        pic.0.height = h;
        // SAFETY: `img` holds `height` rows of `width * 4` bytes. libwebp copies them.
        if unsafe { sys::WebPPictureImportRGBA(&mut pic.0, img.as_ptr(), w * 4) } == 0 {
            return Err("libwebp could not import frame".into());
        }
        // SAFETY: live encoder, imported picture, validated config.
        if unsafe { sys::WebPAnimEncoderAdd(enc.0.as_ptr(), &mut pic.0, timestamp, &config) } == 0 {
            return Err(enc.error());
        }
        timestamp = timestamp.saturating_add(c_int::try_from(*delay).unwrap_or(c_int::MAX));
    }
    // A null frame marks the end. Its timestamp sets the last frame's duration.
    // SAFETY: live encoder. A null picture and config are allowed here.
    if unsafe { sys::WebPAnimEncoderAdd(enc.0.as_ptr(), ptr::null_mut(), timestamp, ptr::null()) }
        == 0
    {
        return Err(enc.error());
    }
    let mut data = sys::WebPData::default();
    // SAFETY: live encoder. On success `data` owns a libwebp allocation.
    if unsafe { sys::WebPAnimEncoderAssemble(enc.0.as_ptr(), &mut data) } == 0 {
        return Err(enc.error());
    }
    // SAFETY: `data` describes a valid buffer, which we copy and then free.
    let bytes = unsafe { std::slice::from_raw_parts(data.bytes, data.size) }.to_vec();
    unsafe { sys::WebPDataClear(&mut data) };
    Ok(bytes)
}
