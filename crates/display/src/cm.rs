//! Minimal CoreMedia / CoreFoundation FFI: build H.264 sample buffers.
use std::ffi::c_void;
use std::ptr::{null, null_mut};

pub type OSStatus = i32;
pub type CFTypeRef = *const c_void;
pub type CMFormatDescriptionRef = *const c_void;
pub type CMBlockBufferRef = *mut c_void;
pub type CMSampleBufferRef = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CMTime {
    pub value: i64,
    pub timescale: i32,
    pub flags: u32,
    pub epoch: i64,
}

impl CMTime {
    const VALID: u32 = 1;
    pub fn micros(us: u64) -> Self {
        CMTime { value: us as i64, timescale: 1_000_000, flags: Self::VALID, epoch: 0 }
    }
    pub fn invalid() -> Self {
        CMTime { value: 0, timescale: 0, flags: 0, epoch: 0 }
    }
}

#[repr(C)]
pub struct CMSampleTimingInfo {
    pub duration: CMTime,
    pub presentation_time_stamp: CMTime,
    pub decode_time_stamp: CMTime,
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    static kCFBooleanTrue: CFTypeRef;
    fn CFRelease(cf: CFTypeRef);
    fn CFArrayGetValueAtIndex(array: CFTypeRef, idx: isize) -> CFTypeRef;
    fn CFDictionarySetValue(dict: *mut c_void, key: CFTypeRef, value: CFTypeRef);
}

#[link(name = "CoreMedia", kind = "framework")]
extern "C" {
    static kCMSampleAttachmentKey_DisplayImmediately: CFTypeRef;

    fn CMVideoFormatDescriptionCreateFromH264ParameterSets(
        allocator: CFTypeRef,
        parameter_set_count: usize,
        parameter_set_pointers: *const *const u8,
        parameter_set_sizes: *const usize,
        nal_unit_header_length: i32,
        format_description_out: *mut CMFormatDescriptionRef,
    ) -> OSStatus;

    fn CMBlockBufferCreateWithMemoryBlock(
        structure_allocator: CFTypeRef,
        memory_block: *mut c_void,
        block_length: usize,
        block_allocator: CFTypeRef,
        custom_block_source: *const c_void,
        offset_to_data: usize,
        data_length: usize,
        flags: u32,
        block_buffer_out: *mut CMBlockBufferRef,
    ) -> OSStatus;

    fn CMBlockBufferReplaceDataBytes(
        source_bytes: *const c_void,
        destination_buffer: CMBlockBufferRef,
        offset_into_destination: usize,
        data_length: usize,
    ) -> OSStatus;

    fn CMSampleBufferCreateReady(
        allocator: CFTypeRef,
        data_buffer: CMBlockBufferRef,
        format_description: CMFormatDescriptionRef,
        num_samples: isize,
        num_sample_timing_entries: isize,
        sample_timing_array: *const CMSampleTimingInfo,
        num_sample_size_entries: isize,
        sample_size_array: *const usize,
        sample_buffer_out: *mut CMSampleBufferRef,
    ) -> OSStatus;

    fn CMSampleBufferGetSampleAttachmentsArray(sbuf: CMSampleBufferRef, create_if_necessary: bool) -> CFTypeRef;
}

const ASSURE_MEMORY_NOW: u32 = 1;

/// An owned CoreFoundation object (released on drop).
pub struct Cf(CFTypeRef);

unsafe impl Send for Cf {}

impl Cf {
    pub fn as_ptr(&self) -> CFTypeRef {
        self.0
    }
}

impl Drop for Cf {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: we own one retain count on this object.
            unsafe { CFRelease(self.0) };
        }
    }
}

/// Format description from raw SPS/PPS NAL units (no start codes), AVCC 4-byte lengths.
pub fn format_description(sps: &[u8], pps: &[u8]) -> Result<Cf, OSStatus> {
    let ptrs = [sps.as_ptr(), pps.as_ptr()];
    let sizes = [sps.len(), pps.len()];
    let mut out: CMFormatDescriptionRef = null();
    // SAFETY: pointers/sizes describe live slices for the duration of the call.
    let st = unsafe {
        CMVideoFormatDescriptionCreateFromH264ParameterSets(null(), 2, ptrs.as_ptr(), sizes.as_ptr(), 4, &mut out)
    };
    if st != 0 || out.is_null() {
        return Err(st);
    }
    Ok(Cf(out))
}

/// A displayable-immediately sample from one AVCC access unit.
pub fn sample_buffer(fmt: &Cf, avcc: &[u8], pts_us: u64) -> Result<Cf, OSStatus> {
    let len = avcc.len();
    let mut block: CMBlockBufferRef = null_mut();
    // SAFETY: null memory_block + AssureMemoryNow makes CoreMedia allocate `len` bytes.
    let st = unsafe {
        CMBlockBufferCreateWithMemoryBlock(null(), null_mut(), len, null(), null(), 0, len, ASSURE_MEMORY_NOW, &mut block)
    };
    if st != 0 {
        return Err(st);
    }
    let block = Cf(block as CFTypeRef);
    // SAFETY: destination has `len` bytes; source is a live slice of `len` bytes.
    let st = unsafe { CMBlockBufferReplaceDataBytes(avcc.as_ptr().cast(), block.0 as CMBlockBufferRef, 0, len) };
    if st != 0 {
        return Err(st);
    }
    let timing = CMSampleTimingInfo {
        duration: CMTime::invalid(),
        presentation_time_stamp: CMTime::micros(pts_us),
        decode_time_stamp: CMTime::invalid(),
    };
    let mut sb: CMSampleBufferRef = null_mut();
    // SAFETY: all pointers are valid for the call; the sample buffer retains `block`/`fmt`.
    let st = unsafe {
        CMSampleBufferCreateReady(null(), block.0 as CMBlockBufferRef, fmt.0, 1, 1, &timing, 1, &len, &mut sb)
    };
    if st != 0 || sb.is_null() {
        return Err(st);
    }
    let sample = Cf(sb as CFTypeRef);
    // SAFETY: attachments array/dictionary are owned by the sample buffer.
    unsafe {
        let arr = CMSampleBufferGetSampleAttachmentsArray(sb, true);
        if !arr.is_null() {
            let dict = CFArrayGetValueAtIndex(arr, 0) as *mut c_void;
            if !dict.is_null() {
                CFDictionarySetValue(dict, kCMSampleAttachmentKey_DisplayImmediately, kCFBooleanTrue);
            }
        }
    }
    Ok(sample)
}
