//! AVPacket interface.
//!
//! A "packet" in the FFmpeg terminology is an encoded part of an elementary
//! stream (i.e. audio or video stream).

use std::{
    os::raw::{c_int, c_void},
    ptr, slice,
};

use crate::{
    codec::VideoCodecParameters,
    time::{TimeBase, Timestamp},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoCodecKind {
    H264,
    Hevc,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RandomAccessKind {
    None,
    Idr,
    Cra,
    Bla,
    RecoveryPoint,
    Unknown,
}

impl RandomAccessKind {
    pub fn is_irap(self) -> bool {
        matches!(self, Self::Idr | Self::Cra | Self::Bla)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeadingPictureKind {
    None,
    Rasl,
    Radl,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoAccessUnitInfo {
    pub codec: VideoCodecKind,
    pub primary_nal_type: u8,
    pub random_access: RandomAccessKind,
    pub leading_picture: LeadingPictureKind,
}

impl VideoAccessUnitInfo {
    pub fn is_irap(&self) -> bool {
        self.random_access.is_irap()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeekSampleTreatment {
    Normal,
    DecodeOnly,
    SaveForFuture,
    DisplayNow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SeekSampleHints {
    pub treatment: SeekSampleTreatment,
    pub hevc_sync_nal_type: Option<u8>,
}

impl Default for SeekSampleHints {
    fn default() -> Self {
        Self {
            treatment: SeekSampleTreatment::Normal,
            hevc_sync_nal_type: None,
        }
    }
}

fn video_codec_kind(params: &VideoCodecParameters) -> Option<VideoCodecKind> {
    match params.decoder_name()? {
        "h264" => Some(VideoCodecKind::H264),
        "hevc" | "h265" => Some(VideoCodecKind::Hevc),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VideoPacketFormat {
    AnnexB,
    LengthPrefixed { nal_length_size: usize },
}

fn nal_length_size(codec: VideoCodecKind, extradata: &[u8]) -> Option<usize> {
    match codec {
        VideoCodecKind::H264 => {
            if extradata.len() >= 5 && extradata[0] == 1 {
                Some(((extradata[4] & 0x03) + 1) as usize)
            } else {
                None
            }
        }
        VideoCodecKind::Hevc => {
            if extradata.len() >= 22 && extradata[0] == 1 {
                Some(((extradata[21] & 0x03) + 1) as usize)
            } else {
                None
            }
        }
    }
}

fn packet_format(codec: VideoCodecKind, extradata: Option<&[u8]>) -> VideoPacketFormat {
    if let Some(length_size) = extradata.and_then(|extra| nal_length_size(codec, extra)) {
        return VideoPacketFormat::LengthPrefixed {
            nal_length_size: length_size,
        };
    }

    VideoPacketFormat::AnnexB
}

fn find_start_code(data: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut i = from;
    while i + 3 <= data.len() {
        if i + 4 <= data.len()
            && data[i] == 0
            && data[i + 1] == 0
            && data[i + 2] == 0
            && data[i + 3] == 1
        {
            return Some((i, 4));
        }
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            return Some((i, 3));
        }
        i += 1;
    }
    None
}

fn annex_b_nal_units<'a>(data: &'a [u8]) -> Vec<&'a [u8]> {
    let mut units = Vec::new();
    let mut cursor = 0;

    while let Some((start, prefix_len)) = find_start_code(data, cursor) {
        let nal_start = start + prefix_len;
        let next_start = find_start_code(data, nal_start)
            .map(|(idx, _)| idx)
            .unwrap_or(data.len());
        if nal_start < next_start {
            units.push(&data[nal_start..next_start]);
        }
        cursor = next_start;
    }

    units
}

fn length_prefixed_nal_units<'a>(data: &'a [u8], nal_length_size: usize) -> Vec<&'a [u8]> {
    let mut units = Vec::new();
    let mut cursor = 0;

    while cursor + nal_length_size <= data.len() {
        let mut nal_len = 0usize;
        for &byte in &data[cursor..cursor + nal_length_size] {
            nal_len = (nal_len << 8) | byte as usize;
        }
        cursor += nal_length_size;

        if nal_len == 0 || cursor + nal_len > data.len() {
            break;
        }

        units.push(&data[cursor..cursor + nal_len]);
        cursor += nal_len;
    }

    units
}

fn nal_units<'a>(
    codec: VideoCodecKind,
    data: &'a [u8],
    extradata: Option<&[u8]>,
) -> Vec<&'a [u8]> {
    if data.is_empty() {
        return Vec::new();
    }

    match packet_format(codec, extradata) {
        VideoPacketFormat::LengthPrefixed { nal_length_size } => {
            length_prefixed_nal_units(data, nal_length_size)
        }
        VideoPacketFormat::AnnexB => annex_b_nal_units(data),
    }
}

fn classify_h264_access_unit(nal_type: u8) -> Option<VideoAccessUnitInfo> {
    if !(1..=5).contains(&nal_type) {
        return None;
    }

    let random_access = if nal_type == 5 {
        RandomAccessKind::Idr
    } else {
        RandomAccessKind::None
    };

    Some(VideoAccessUnitInfo {
        codec: VideoCodecKind::H264,
        primary_nal_type: nal_type,
        random_access,
        leading_picture: LeadingPictureKind::None,
    })
}

fn classify_hevc_access_unit(nal_type: u8) -> Option<VideoAccessUnitInfo> {
    if nal_type > 31 {
        return None;
    }

    let random_access = match nal_type {
        16..=18 => RandomAccessKind::Bla,
        19..=20 => RandomAccessKind::Idr,
        21 => RandomAccessKind::Cra,
        _ => RandomAccessKind::None,
    };

    let leading_picture = match nal_type {
        6 | 7 => LeadingPictureKind::Radl,
        8 | 9 => LeadingPictureKind::Rasl,
        _ => LeadingPictureKind::None,
    };

    Some(VideoAccessUnitInfo {
        codec: VideoCodecKind::Hevc,
        primary_nal_type: nal_type,
        random_access,
        leading_picture,
    })
}

extern "C" {
    fn ffw_packet_alloc() -> *mut c_void;
    fn ffw_packet_new(size: c_int) -> *mut c_void;
    fn ffw_packet_clone(src: *const c_void) -> *mut c_void;
    fn ffw_packet_free(packet: *mut c_void);
    fn ffw_packet_get_size(packet: *const c_void) -> c_int;
    fn ffw_packet_get_duration(packet: *const c_void) -> i64;
    fn ffw_packet_set_duration(packet: *mut c_void, time: i64);
    fn ffw_packet_get_data(packet: *mut c_void) -> *mut c_void;
    fn ffw_packet_get_pts(packet: *const c_void) -> i64;
    fn ffw_packet_set_pts(packet: *mut c_void, pts: i64);
    fn ffw_packet_get_dts(packet: *const c_void) -> i64;
    fn ffw_packet_set_dts(packet: *mut c_void, pts: i64);
    fn ffw_packet_set_time_base(packet: *mut c_void, num: u32, den: u32);
    fn ffw_packet_is_key(packet: *const c_void) -> c_int;
    fn ffw_packet_set_key(packet: *mut c_void, key: c_int);
    fn ffw_packet_is_discard(packet: *const c_void) -> c_int;
    fn ffw_packet_set_discard(packet: *mut c_void, discard: c_int);
    fn ffw_packet_get_stream_index(packet: *const c_void) -> c_int;
    fn ffw_packet_set_stream_index(packet: *mut c_void, index: c_int);
    fn ffw_packet_make_writable(packet: *mut c_void) -> c_int;
}

/// Packet with mutable data.
pub struct PacketMut {
    ptr: *mut c_void,
    time_base: TimeBase,
    seek_hints: SeekSampleHints,
}

impl PacketMut {
    /// Create a new packet of a given size. The time base of the packet will
    /// be in microseconds.
    pub fn new(size: usize) -> Self {
        unsafe {
            let ptr = if size == 0 {
                ffw_packet_alloc()
            } else {
                ffw_packet_new(size as c_int)
            };

            if ptr.is_null() {
                panic!("unable to allocate a packet");
            }

            Self {
                ptr,
                time_base: TimeBase::MICROSECONDS,
                seek_hints: SeekSampleHints::default(),
            }
        }
    }

    /// Get stream index.
    pub fn stream_index(&self) -> usize {
        unsafe { ffw_packet_get_stream_index(self.ptr) as _ }
    }

    /// Set stream index.
    pub fn with_stream_index(self, index: usize) -> Self {
        unsafe { ffw_packet_set_stream_index(self.ptr, index as _) }

        self
    }

    /// Get packet time base.
    pub fn time_base(&self) -> TimeBase {
        self.time_base
    }

    /// Set packet time base. (This will rescale the current timestamps into a
    /// given time base.)
    pub fn with_time_base(mut self, time_base: TimeBase) -> Self {
        let new_pts = self.pts().with_time_base(time_base);
        let new_dts = self.dts().with_time_base(time_base);

        unsafe {
            ffw_packet_set_pts(self.ptr, new_pts.timestamp());
            ffw_packet_set_dts(self.ptr, new_dts.timestamp());
        }

        self.time_base = time_base;

        self
    }

    /// Get packet presentation timestamp.
    pub fn pts(&self) -> Timestamp {
        let pts = unsafe { ffw_packet_get_pts(self.ptr) };

        Timestamp::new(pts, self.time_base)
    }

    /// Set packet presentation timestamp.
    pub fn with_pts(self, pts: Timestamp) -> Self {
        let pts = pts.with_time_base(self.time_base);

        unsafe { ffw_packet_set_pts(self.ptr, pts.timestamp()) }

        self
    }

    /// Get packet decoding timestamp.
    pub fn dts(&self) -> Timestamp {
        let dts = unsafe { ffw_packet_get_dts(self.ptr) };

        Timestamp::new(dts, self.time_base)
    }

    /// Set packet decoding timestamp.
    pub fn with_dts(self, dts: Timestamp) -> Self {
        let dts = dts.with_time_base(self.time_base);

        unsafe { ffw_packet_set_dts(self.ptr, dts.timestamp()) }

        self
    }

    /// Get the packet duration.
    pub fn duration(&self) -> Timestamp {
        let time = unsafe { ffw_packet_get_duration(self.ptr) };

        Timestamp::new(time, self.time_base)
    }

    /// Set the packet duration.
    pub fn with_duration(self, duration: Timestamp) -> Self {
        let dur = duration.with_time_base(self.time_base);

        unsafe { ffw_packet_set_duration(self.ptr, dur.timestamp()) }

        self
    }

    /// Check if the key flag is set.
    pub fn is_key(&self) -> bool {
        unsafe { ffw_packet_is_key(self.ptr) != 0 }
    }

    /// Set or unset the key flag.
    pub fn with_key_flag(self, key: bool) -> Self {
        unsafe { ffw_packet_set_key(self.ptr, key as _) }

        self
    }

    /// Get packet data.
    pub fn data(&self) -> &[u8] {
        unsafe {
            let data = ffw_packet_get_data(self.ptr) as *const u8;
            let size = ffw_packet_get_size(self.ptr) as usize;

            if data.is_null() {
                &[]
            } else {
                slice::from_raw_parts(data, size)
            }
        }
    }

    /// Get mutable reference to the packet data.
    pub fn data_mut(&mut self) -> &mut [u8] {
        unsafe {
            let data = ffw_packet_get_data(self.ptr) as *mut u8;
            let size = ffw_packet_get_size(self.ptr) as usize;

            if data.is_null() {
                &mut []
            } else {
                slice::from_raw_parts_mut(data, size)
            }
        }
    }

    pub fn with_seek_treatment(mut self, treatment: SeekSampleTreatment) -> Self {
        self.seek_hints.treatment = treatment;
        self
    }

    pub fn with_hevc_sync_nal_type(mut self, nal_type: Option<u8>) -> Self {
        self.seek_hints.hevc_sync_nal_type = nal_type;
        self
    }

    pub fn seek_hints(&self) -> SeekSampleHints {
        self.seek_hints
    }

    /// Make the packet immutable.
    pub fn freeze(mut self) -> Packet {
        let ptr = self.ptr;
        let seek_hints = self.seek_hints;

        self.ptr = ptr::null_mut();

        Packet {
            ptr,
            time_base: self.time_base,
            seek_hints,
        }
    }
}

impl Drop for PacketMut {
    fn drop(&mut self) {
        unsafe { ffw_packet_free(self.ptr) }
    }
}

impl<T> From<T> for PacketMut
where
    T: AsRef<[u8]>,
{
    fn from(data: T) -> Self {
        let data = data.as_ref();

        let mut packet = Self::new(data.len());

        packet.data_mut().copy_from_slice(data);

        packet
    }
}

unsafe impl Send for PacketMut {}
unsafe impl Sync for PacketMut {}

/// Packet with immutable data.
pub struct Packet {
    ptr: *mut c_void,
    time_base: TimeBase,
    seek_hints: SeekSampleHints,
}

impl Packet {
    /// Create a new immutable packet from its raw representation.
    pub(crate) unsafe fn from_raw_ptr(ptr: *mut c_void, time_base: TimeBase) -> Self {
        Packet {
            ptr,
            time_base,
            seek_hints: SeekSampleHints::default(),
        }
    }

    /// Get stream index.
    pub fn stream_index(&self) -> usize {
        unsafe { ffw_packet_get_stream_index(self.ptr) as _ }
    }

    /// Set stream index.
    pub fn with_stream_index(self, index: usize) -> Packet {
        unsafe { ffw_packet_set_stream_index(self.ptr, index as _) }

        self
    }

    /// Get packet time base.
    pub fn time_base(&self) -> TimeBase {
        self.time_base
    }

    /// Set packet time base. (This will rescale the current timestamps into a
    /// given time base.)
    pub fn with_time_base(mut self, time_base: TimeBase) -> Self {
        let new_pts = self.pts().with_time_base(time_base);
        let new_dts = self.dts().with_time_base(time_base);

        unsafe {
            ffw_packet_set_pts(self.ptr, new_pts.timestamp());
            ffw_packet_set_dts(self.ptr, new_dts.timestamp());
            ffw_packet_set_time_base(self.ptr, time_base.num(), time_base.den());
        }

        self.time_base = time_base;

        self
    }

    /// Get packet presentation timestamp.
    pub fn pts(&self) -> Timestamp {
        let pts = unsafe { ffw_packet_get_pts(self.ptr) };

        Timestamp::new(pts, self.time_base)
    }

    /// Set packet presentation timestamp.
    pub fn with_pts(self, pts: Timestamp) -> Self {
        let pts = pts.with_time_base(self.time_base);

        unsafe { ffw_packet_set_pts(self.ptr, pts.timestamp()) }

        self
    }

    /// Get packet decoding timestamp.
    pub fn dts(&self) -> Timestamp {
        let dts = unsafe { ffw_packet_get_dts(self.ptr) };

        Timestamp::new(dts, self.time_base)
    }

    /// Set packet decoding timestamp.
    pub fn with_dts(self, dts: Timestamp) -> Self {
        let dts = dts.with_time_base(self.time_base);

        unsafe { ffw_packet_set_dts(self.ptr, dts.timestamp()) }

        self
    }

    /// Get the packet duration.
    pub fn duration(&self) -> Timestamp {
        let time = unsafe { ffw_packet_get_duration(self.ptr) };

        Timestamp::new(time, self.time_base)
    }

    /// Set the packet duration.
    pub fn with_duration(self, duration: Timestamp) -> Self {
        let dur = duration.with_time_base(self.time_base);

        unsafe { ffw_packet_set_duration(self.ptr, dur.timestamp()) }

        self
    }

    /// Set or clear the discard flag. If true, the this packet can be passed
    /// to the decoder and the decoder will not produce a frame. Useful as an
    /// optimization when packets should be discarded (e.g. during seeking).
    pub fn with_discard_flag(self, discard: bool) -> Self {
        unsafe { ffw_packet_set_discard(self.ptr, discard as _) }

        self
    }

    pub fn with_seek_treatment(mut self, treatment: SeekSampleTreatment) -> Self {
        self.seek_hints.treatment = treatment;
        self
    }

    pub fn with_hevc_sync_nal_type(mut self, nal_type: Option<u8>) -> Self {
        self.seek_hints.hevc_sync_nal_type = nal_type;
        self
    }

    pub fn seek_hints(&self) -> SeekSampleHints {
        self.seek_hints
    }

    /// Check if the discard flag is set.
    pub fn is_discard(&self) -> bool {
        unsafe { ffw_packet_is_discard(self.ptr) != 0 }
    }

    /// Check if the key flag is set.
    pub fn is_key(&self) -> bool {
        unsafe { ffw_packet_is_key(self.ptr) != 0 }
    }

    /// Get raw pointer.
    pub(crate) fn as_ptr(&self) -> *const c_void {
        self.ptr
    }

    /// Get mutable raw pointer. Please note that even though it is required
    /// in some cases to pass a mut pointer to an immutable packet, it is not
    /// allowed to modify packet data in such cases.
    pub(crate) fn as_mut_ptr(&mut self) -> *mut c_void {
        self.ptr
    }

    /// Get packet data.
    pub fn data(&self) -> &[u8] {
        unsafe {
            let data = ffw_packet_get_data(self.ptr) as *const u8;
            let size = ffw_packet_get_size(self.ptr) as usize;

            if data.is_null() {
                &[]
            } else {
                slice::from_raw_parts(data, size)
            }
        }
    }

    /// Parse supported video packet formats and return access-unit metadata
    /// for seek-priming decisions.
    pub fn video_access_unit_info(
        &self,
        codec_params: &VideoCodecParameters,
    ) -> Option<VideoAccessUnitInfo> {
        let codec = video_codec_kind(codec_params)?;
        let extradata = codec_params.extradata();
        let nals = nal_units(codec, self.data(), extradata);

        for nal in nals {
            let info = match codec {
                VideoCodecKind::H264 => {
                    let nal_type = *nal.first()? & 0x1F;
                    classify_h264_access_unit(nal_type)
                }
                VideoCodecKind::Hevc => {
                    if nal.len() < 2 {
                        None
                    } else {
                        let nal_type = (nal[0] >> 1) & 0x3F;
                        classify_hevc_access_unit(nal_type)
                    }
                }
            };

            if info.is_some() {
                return info;
            }
        }

        None
    }

    /// Make this packet mutable. If there are no other references to the
    /// packet data, the mutable packet will be created without copying the
    /// data.
    pub fn into_mut(mut self) -> PacketMut {
        let res = unsafe { ffw_packet_make_writable(self.ptr) };

        if res < 0 {
            panic!("unable to make the packet mutable");
        }

        let ptr = self.ptr;

        self.ptr = ptr::null_mut();

        PacketMut {
            ptr,
            time_base: self.time_base,
            seek_hints: self.seek_hints,
        }
    }
}

impl Clone for Packet {
    fn clone(&self) -> Packet {
        let ptr = unsafe { ffw_packet_clone(self.ptr) };

        if ptr.is_null() {
            panic!("unable to clone a packet");
        }

        Packet {
            ptr,
            time_base: self.time_base,
            seek_hints: self.seek_hints,
        }
    }
}

impl Drop for Packet {
    fn drop(&mut self) {
        unsafe { ffw_packet_free(self.ptr) }
    }
}

unsafe impl Send for Packet {}
unsafe impl Sync for Packet {}

#[cfg(test)]
mod tests {
    use super::*;

    fn hevc_codec_params_with_hvcc_len4() -> VideoCodecParameters {
        let mut hvcc = vec![0u8; 22];
        hvcc[0] = 1;
        hvcc[21] = 3;
        VideoCodecParameters::builder("hevc")
            .unwrap()
            .extradata(Some(hvcc))
            .build()
    }

    #[test]
    fn parses_hevc_length_prefixed_access_unit_info() {
        let codec_params = hevc_codec_params_with_hvcc_len4();
        let packet = PacketMut::from([0u8, 0, 0, 2, 16, 1]).freeze();

        let info = packet.video_access_unit_info(&codec_params).unwrap();
        assert_eq!(info.codec, VideoCodecKind::Hevc);
        assert_eq!(info.leading_picture, LeadingPictureKind::Rasl);
        assert_eq!(info.random_access, RandomAccessKind::None);
    }

    #[test]
    fn parses_h264_annex_b_access_unit_info() {
        let codec_params = VideoCodecParameters::builder("h264").unwrap().build();
        let packet = PacketMut::from([0u8, 0, 1, 0x65]).freeze();

        let info = packet.video_access_unit_info(&codec_params).unwrap();
        assert_eq!(info.codec, VideoCodecKind::H264);
        assert_eq!(info.random_access, RandomAccessKind::Idr);
        assert_eq!(info.leading_picture, LeadingPictureKind::None);
    }
}
