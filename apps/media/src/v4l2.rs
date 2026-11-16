//! The kernel interface of the hardware decoder: V4L2 stateless H.264 ("request API") as the
//! rkvdec driver implements it (Linux 7.3, `rockchip,rk3228-vdec`).
//!
//! Structs mirror `include/uapi/linux/videodev2.h`, `v4l2-controls.h` and `media.h` with 64-bit
//! timestamps (the kernel's native layout on 32-bit ARM). Their sizes, offsets and the ioctl
//! numbers were read from the 7.3.4 headers with the cross compiler on 14 Nov 2026 and are
//! asserted below for the dongle's target, so a mismatch fails the build rather than the decode.

use std::mem::size_of;

use crate::h264::{Picture, Pps, ScalingLists, SliceType, Sps};

pub const BUF_TYPE_CAPTURE_MPLANE: u32 = 9;
pub const BUF_TYPE_OUTPUT_MPLANE: u32 = 10;
pub const MEMORY_MMAP: u32 = 1;
pub const FIELD_NONE: u32 = 1;
pub const PIX_FMT_H264_SLICE: u32 = u32::from_le_bytes(*b"S264");
pub const PIX_FMT_NV12: u32 = u32::from_le_bytes(*b"NV12");
pub const CAP_VIDEO_M2M_MPLANE: u32 = 0x4000;
pub const CAP_STREAMING: u32 = 0x0400_0000;
pub const BUF_FLAG_ERROR: u32 = 0x40;
pub const BUF_FLAG_REQUEST_FD: u32 = 0x0080_0000;
pub const CTRL_WHICH_REQUEST_VAL: u32 = 0x0f01_0000;
pub const CID_H264_SPS: u32 = 0x00a4_0902;
pub const CID_H264_PPS: u32 = 0x00a4_0903;
pub const CID_H264_SCALING_MATRIX: u32 = 0x00a4_0904;
pub const CID_H264_DECODE_PARAMS: u32 = 0x00a4_0907;

const fn ioc(direction: u32, kind: u8, number: u8, size: usize) -> u32 {
    direction << 30 | (size as u32) << 16 | (kind as u32) << 8 | number as u32
}
const READ: u32 = 2;
const WRITE: u32 = 1;

pub const VIDIOC_QUERYCAP: u32 = ioc(READ, b'V', 0, size_of::<Capability>());
pub const VIDIOC_G_FMT: u32 = ioc(READ | WRITE, b'V', 4, size_of::<Format>());
pub const VIDIOC_S_FMT: u32 = ioc(READ | WRITE, b'V', 5, size_of::<Format>());
pub const VIDIOC_REQBUFS: u32 = ioc(READ | WRITE, b'V', 8, size_of::<RequestBuffers>());
pub const VIDIOC_QUERYBUF: u32 = ioc(READ | WRITE, b'V', 9, size_of::<Buffer>());
pub const VIDIOC_QBUF: u32 = ioc(READ | WRITE, b'V', 15, size_of::<Buffer>());
pub const VIDIOC_EXPBUF: u32 = ioc(READ | WRITE, b'V', 16, size_of::<ExportBuffer>());
pub const VIDIOC_DQBUF: u32 = ioc(READ | WRITE, b'V', 17, size_of::<Buffer>());
pub const VIDIOC_STREAMON: u32 = ioc(WRITE, b'V', 18, 4);
pub const VIDIOC_STREAMOFF: u32 = ioc(WRITE, b'V', 19, 4);
pub const VIDIOC_S_EXT_CTRLS: u32 = ioc(READ | WRITE, b'V', 72, size_of::<ExtControls>());
pub const MEDIA_IOC_DEVICE_INFO: u32 = ioc(READ | WRITE, b'|', 0, size_of::<MediaDeviceInfo>());
pub const MEDIA_IOC_REQUEST_ALLOC: u32 = ioc(READ, b'|', 5, 4);
pub const MEDIA_REQUEST_IOC_QUEUE: u32 = ioc(0, b'|', 0x80, 0);
pub const MEDIA_REQUEST_IOC_REINIT: u32 = ioc(0, b'|', 0x81, 0);

#[repr(C)]
pub struct Capability {
    pub driver: [u8; 16],
    pub card: [u8; 32],
    pub bus_info: [u8; 32],
    pub version: u32,
    pub capabilities: u32,
    pub device_caps: u32,
    pub reserved: [u32; 3],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct PlanePixFormat {
    pub sizeimage: u32,
    pub bytesperline: u32,
    pub reserved: [u16; 6],
}

/// `struct v4l2_pix_format_mplane` (packed in the kernel; every field is naturally aligned
/// until the single bytes at the end, which is why `repr(C, packed)` is needed for the size).
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct PixFormatMplane {
    pub width: u32,
    pub height: u32,
    pub pixelformat: u32,
    pub field: u32,
    pub colorspace: u32,
    pub plane_fmt: [PlanePixFormat; 8],
    pub num_planes: u8,
    pub flags: u8,
    pub ycbcr_enc: u8,
    pub quantization: u8,
    pub xfer_func: u8,
    pub reserved: [u8; 7],
}

/// `struct v4l2_format`: the type and a 200-byte union, of which only `pix_mp` is used here.
#[repr(C)]
pub struct Format {
    pub kind: u32,
    pub pix_mp: PixFormatMplane,
    pub padding: [u8; 8],
}

#[repr(C)]
pub struct RequestBuffers {
    pub count: u32,
    pub kind: u32,
    pub memory: u32,
    pub capabilities: u32,
    pub flags: u8,
    pub reserved: [u8; 3],
}

/// `struct v4l2_plane`. `m` is a union of `__u32 mem_offset`, `unsigned long userptr`, `__s32 fd`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Plane {
    pub bytesused: u32,
    pub length: u32,
    pub m: usize,
    pub data_offset: u32,
    pub reserved: [u32; 11],
}

/// `struct v4l2_buffer` with the kernel's 64-bit timeval. `m` holds the pointer to the planes.
#[repr(C)]
pub struct Buffer {
    pub index: u32,
    pub kind: u32,
    pub bytesused: u32,
    pub flags: u32,
    pub field: u32,
    pub tv_sec: i64,
    pub tv_usec: i64,
    pub timecode: [u32; 4],
    pub sequence: u32,
    pub memory: u32,
    pub m: usize,
    pub length: u32,
    pub reserved2: u32,
    pub request_fd: i32,
}

#[repr(C)]
pub struct ExportBuffer {
    pub kind: u32,
    pub index: u32,
    pub plane: u32,
    pub flags: u32,
    pub fd: i32,
    pub reserved: [u32; 11],
}

/// `struct v4l2_ext_control` (packed): id, size, reserved, then a 64-bit union holding a pointer.
#[repr(C, packed)]
pub struct ExtControl {
    pub id: u32,
    pub size: u32,
    pub reserved2: u32,
    pub ptr: u64,
}

#[repr(C)]
pub struct ExtControls {
    pub which: u32,
    pub count: u32,
    pub error_idx: u32,
    pub request_fd: i32,
    pub reserved: u32,
    pub controls: *mut ExtControl,
}

#[repr(C)]
pub struct MediaDeviceInfo {
    pub driver: [u8; 16],
    pub model: [u8; 32],
    pub serial: [u8; 40],
    pub bus_info: [u8; 32],
    pub media_version: u32,
    pub hw_revision: u32,
    pub driver_version: u32,
    pub reserved: [u32; 31],
}

#[repr(C)]
pub struct H264Sps {
    pub profile_idc: u8,
    pub constraint_set_flags: u8,
    pub level_idc: u8,
    pub seq_parameter_set_id: u8,
    pub chroma_format_idc: u8,
    pub bit_depth_luma_minus8: u8,
    pub bit_depth_chroma_minus8: u8,
    pub log2_max_frame_num_minus4: u8,
    pub pic_order_cnt_type: u8,
    pub log2_max_pic_order_cnt_lsb_minus4: u8,
    pub max_num_ref_frames: u8,
    pub num_ref_frames_in_pic_order_cnt_cycle: u8,
    pub offset_for_ref_frame: [i32; 255],
    pub offset_for_non_ref_pic: i32,
    pub offset_for_top_to_bottom_field: i32,
    pub pic_width_in_mbs_minus1: u16,
    pub pic_height_in_map_units_minus1: u16,
    pub flags: u32,
}

#[repr(C)]
#[derive(Default)]
pub struct H264Pps {
    pub pic_parameter_set_id: u8,
    pub seq_parameter_set_id: u8,
    pub num_slice_groups_minus1: u8,
    pub num_ref_idx_l0_default_active_minus1: u8,
    pub num_ref_idx_l1_default_active_minus1: u8,
    pub weighted_bipred_idc: u8,
    pub pic_init_qp_minus26: i8,
    pub pic_init_qs_minus26: i8,
    pub chroma_qp_index_offset: i8,
    pub second_chroma_qp_index_offset: i8,
    pub flags: u16,
}

#[repr(C)]
pub struct H264ScalingMatrix {
    pub scaling_list_4x4: [[u8; 16]; 6],
    pub scaling_list_8x8: [[u8; 64]; 6],
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct H264DpbEntry {
    pub reference_ts: u64,
    pub pic_num: u32,
    pub frame_num: u16,
    pub fields: u8,
    pub reserved: [u8; 5],
    pub top_field_order_cnt: i32,
    pub bottom_field_order_cnt: i32,
    pub flags: u32,
}

#[repr(C)]
#[derive(Default)]
pub struct H264DecodeParams {
    pub dpb: [H264DpbEntry; 16],
    pub nal_ref_idc: u16,
    pub frame_num: u16,
    pub top_field_order_cnt: i32,
    pub bottom_field_order_cnt: i32,
    pub idr_pic_id: u16,
    pub pic_order_cnt_lsb: u16,
    pub delta_pic_order_cnt_bottom: i32,
    pub delta_pic_order_cnt0: i32,
    pub delta_pic_order_cnt1: i32,
    pub dec_ref_pic_marking_bit_size: u32,
    pub pic_order_cnt_bit_size: u32,
    pub slice_group_change_cycle: u32,
    pub reserved: u32,
    pub flags: u32,
}

// Layout facts that do not depend on pointer width hold on every target.
const _: () = {
    assert!(size_of::<Capability>() == 104);
    assert!(size_of::<PixFormatMplane>() == 192);
    assert!(size_of::<Format>() == 204);
    assert!(size_of::<RequestBuffers>() == 20);
    assert!(size_of::<ExportBuffer>() == 64);
    assert!(size_of::<ExtControl>() == 20);
    assert!(size_of::<MediaDeviceInfo>() == 256);
    assert!(size_of::<H264Sps>() == 1048);
    assert!(size_of::<H264Pps>() == 12);
    assert!(size_of::<H264ScalingMatrix>() == 480);
    assert!(size_of::<H264DpbEntry>() == 32);
    assert!(size_of::<H264DecodeParams>() == 560);
    assert!(std::mem::offset_of!(H264Sps, flags) == 1044);
    assert!(std::mem::offset_of!(H264DecodeParams, nal_ref_idc) == 512);
    assert!(std::mem::offset_of!(H264DecodeParams, flags) == 556);
};

// The dongle's target: the values the 7.3.4 headers give arm-linux-gnueabihf (-D_TIME_BITS=64).
#[cfg(target_arch = "arm")]
const _: () = {
    assert!(size_of::<Buffer>() == 80);
    assert!(std::mem::offset_of!(Buffer, tv_sec) == 24);
    assert!(std::mem::offset_of!(Buffer, memory) == 60);
    assert!(std::mem::offset_of!(Buffer, m) == 64);
    assert!(std::mem::offset_of!(Buffer, request_fd) == 76);
    assert!(size_of::<Plane>() == 60);
    assert!(std::mem::offset_of!(Plane, data_offset) == 12);
    assert!(size_of::<ExtControls>() == 24);
    assert!(std::mem::offset_of!(ExtControls, controls) == 20);
    assert!(VIDIOC_QUERYCAP == 0x8068_5600);
    assert!(VIDIOC_G_FMT == 0xc0cc_5604);
    assert!(VIDIOC_S_FMT == 0xc0cc_5605);
    assert!(VIDIOC_REQBUFS == 0xc014_5608);
    assert!(VIDIOC_QUERYBUF == 0xc050_5609);
    assert!(VIDIOC_QBUF == 0xc050_560f);
    assert!(VIDIOC_EXPBUF == 0xc040_5610);
    assert!(VIDIOC_DQBUF == 0xc050_5611);
    assert!(VIDIOC_STREAMON == 0x4004_5612);
    assert!(VIDIOC_STREAMOFF == 0x4004_5613);
    assert!(VIDIOC_S_EXT_CTRLS == 0xc018_5648);
    assert!(MEDIA_IOC_DEVICE_INFO == 0xc100_7c00);
    assert!(MEDIA_IOC_REQUEST_ALLOC == 0x8004_7c05);
    assert!(MEDIA_REQUEST_IOC_QUEUE == 0x7c80);
    assert!(MEDIA_REQUEST_IOC_REINIT == 0x7c81);
};

const SPS_FLAG_SEPARATE_COLOUR_PLANE: u32 = 0x01;
const SPS_FLAG_QPPRIME_Y_ZERO_TRANSFORM_BYPASS: u32 = 0x02;
const SPS_FLAG_DELTA_PIC_ORDER_ALWAYS_ZERO: u32 = 0x04;
const SPS_FLAG_GAPS_IN_FRAME_NUM_VALUE_ALLOWED: u32 = 0x08;
const SPS_FLAG_FRAME_MBS_ONLY: u32 = 0x10;
const SPS_FLAG_MB_ADAPTIVE_FRAME_FIELD: u32 = 0x20;
const SPS_FLAG_DIRECT_8X8_INFERENCE: u32 = 0x40;

const PPS_FLAG_ENTROPY_CODING_MODE: u16 = 0x01;
const PPS_FLAG_BOTTOM_FIELD_PIC_ORDER_IN_FRAME_PRESENT: u16 = 0x02;
const PPS_FLAG_WEIGHTED_PRED: u16 = 0x04;
const PPS_FLAG_DEBLOCKING_FILTER_CONTROL_PRESENT: u16 = 0x08;
const PPS_FLAG_CONSTRAINED_INTRA_PRED: u16 = 0x10;
const PPS_FLAG_REDUNDANT_PIC_CNT_PRESENT: u16 = 0x20;
const PPS_FLAG_TRANSFORM_8X8_MODE: u16 = 0x40;
const PPS_FLAG_SCALING_MATRIX_PRESENT: u16 = 0x80;

const FRAME_REF: u8 = 0x3;
const DPB_ENTRY_FLAG_VALID: u32 = 0x01;
const DPB_ENTRY_FLAG_ACTIVE: u32 = 0x02;
const DPB_ENTRY_FLAG_LONG_TERM: u32 = 0x04;
const DECODE_PARAM_FLAG_IDR_PIC: u32 = 0x01;
const DECODE_PARAM_FLAG_PFRAME: u32 = 0x08;
const DECODE_PARAM_FLAG_BFRAME: u32 = 0x10;

fn flags<T: Copy + std::ops::BitOr<Output = T> + Default>(pairs: &[(bool, T)]) -> T {
    pairs.iter().filter(|(on, _)| *on).fold(T::default(), |all, (_, flag)| all | *flag)
}

pub fn sps_control(sps: &Sps) -> Box<H264Sps> {
    let mut offsets = [0i32; 255];
    offsets[..sps.offset_for_ref_frame.len()].copy_from_slice(&sps.offset_for_ref_frame);
    Box::new(H264Sps {
        profile_idc: sps.profile_idc,
        constraint_set_flags: sps.constraint_set_flags,
        level_idc: sps.level_idc,
        seq_parameter_set_id: sps.id,
        chroma_format_idc: sps.chroma_format_idc,
        bit_depth_luma_minus8: sps.bit_depth_luma_minus8,
        bit_depth_chroma_minus8: sps.bit_depth_chroma_minus8,
        log2_max_frame_num_minus4: sps.log2_max_frame_num_minus4,
        pic_order_cnt_type: sps.pic_order_cnt_type,
        log2_max_pic_order_cnt_lsb_minus4: sps.log2_max_pic_order_cnt_lsb_minus4,
        max_num_ref_frames: sps.max_num_ref_frames,
        num_ref_frames_in_pic_order_cnt_cycle: sps.offset_for_ref_frame.len() as u8,
        offset_for_ref_frame: offsets,
        offset_for_non_ref_pic: sps.offset_for_non_ref_pic,
        offset_for_top_to_bottom_field: sps.offset_for_top_to_bottom_field,
        pic_width_in_mbs_minus1: sps.pic_width_in_mbs_minus1,
        pic_height_in_map_units_minus1: sps.pic_height_in_map_units_minus1,
        flags: flags(&[
            (sps.separate_colour_plane, SPS_FLAG_SEPARATE_COLOUR_PLANE),
            (sps.qpprime_y_zero_transform_bypass, SPS_FLAG_QPPRIME_Y_ZERO_TRANSFORM_BYPASS),
            (sps.delta_pic_order_always_zero, SPS_FLAG_DELTA_PIC_ORDER_ALWAYS_ZERO),
            (sps.gaps_in_frame_num_allowed, SPS_FLAG_GAPS_IN_FRAME_NUM_VALUE_ALLOWED),
            (sps.frame_mbs_only, SPS_FLAG_FRAME_MBS_ONLY),
            (sps.mb_adaptive_frame_field, SPS_FLAG_MB_ADAPTIVE_FRAME_FIELD),
            (sps.direct_8x8_inference, SPS_FLAG_DIRECT_8X8_INFERENCE),
        ]),
    })
}

/// The PPS control and the scaling lists in effect (the picture's, else the sequence's, else flat).
pub fn pps_controls(pps: &Pps, sps: &Sps) -> (H264Pps, H264ScalingMatrix) {
    let lists = pps.scaling.as_ref().or(sps.scaling.as_ref());
    let control = H264Pps {
        pic_parameter_set_id: pps.id,
        seq_parameter_set_id: pps.sps_id,
        num_slice_groups_minus1: 0,
        num_ref_idx_l0_default_active_minus1: pps.num_ref_idx_l0_default_active_minus1,
        num_ref_idx_l1_default_active_minus1: pps.num_ref_idx_l1_default_active_minus1,
        weighted_bipred_idc: pps.weighted_bipred_idc,
        pic_init_qp_minus26: pps.pic_init_qp_minus26,
        pic_init_qs_minus26: pps.pic_init_qs_minus26,
        chroma_qp_index_offset: pps.chroma_qp_index_offset,
        second_chroma_qp_index_offset: pps.second_chroma_qp_index_offset,
        flags: flags(&[
            (pps.entropy_coding_mode, PPS_FLAG_ENTROPY_CODING_MODE),
            (pps.bottom_field_pic_order_in_frame_present, PPS_FLAG_BOTTOM_FIELD_PIC_ORDER_IN_FRAME_PRESENT),
            (pps.weighted_pred, PPS_FLAG_WEIGHTED_PRED),
            (pps.deblocking_filter_control_present, PPS_FLAG_DEBLOCKING_FILTER_CONTROL_PRESENT),
            (pps.constrained_intra_pred, PPS_FLAG_CONSTRAINED_INTRA_PRED),
            (pps.redundant_pic_cnt_present, PPS_FLAG_REDUNDANT_PIC_CNT_PRESENT),
            (pps.transform_8x8_mode, PPS_FLAG_TRANSFORM_8X8_MODE),
            (lists.is_some(), PPS_FLAG_SCALING_MATRIX_PRESENT),
        ]),
    };
    let ScalingLists { lists4, lists8 } = lists.cloned().unwrap_or(ScalingLists { lists4: [[16; 16]; 6], lists8: [[16; 64]; 6] });
    (control, H264ScalingMatrix { scaling_list_4x4: lists4, scaling_list_8x8: lists8 })
}

/// `Reference::key` is the buffer timestamp in nanoseconds, which is how the kernel finds a
/// reference picture's buffer.
pub fn decode_params(picture: &Picture) -> H264DecodeParams {
    let mut params = H264DecodeParams {
        nal_ref_idc: picture.nal_ref_idc as u16,
        frame_num: picture.frame_num,
        top_field_order_cnt: picture.top_field_order_cnt,
        bottom_field_order_cnt: picture.bottom_field_order_cnt,
        idr_pic_id: picture.idr_pic_id,
        pic_order_cnt_lsb: picture.pic_order_cnt_lsb,
        delta_pic_order_cnt_bottom: picture.delta_pic_order_cnt_bottom,
        delta_pic_order_cnt0: picture.delta_pic_order_cnt[0],
        delta_pic_order_cnt1: picture.delta_pic_order_cnt[1],
        dec_ref_pic_marking_bit_size: picture.dec_ref_pic_marking_bit_size,
        pic_order_cnt_bit_size: picture.pic_order_cnt_bit_size,
        flags: flags(&[
            (picture.idr, DECODE_PARAM_FLAG_IDR_PIC),
            (picture.slice_type == SliceType::P, DECODE_PARAM_FLAG_PFRAME),
            (picture.slice_type == SliceType::B, DECODE_PARAM_FLAG_BFRAME),
        ]),
        ..Default::default()
    };
    for (entry, reference) in params.dpb.iter_mut().zip(&picture.references) {
        *entry = H264DpbEntry {
            reference_ts: reference.key,
            pic_num: reference.long_term.unwrap_or(reference.frame_num as u32),
            frame_num: reference.frame_num,
            fields: FRAME_REF,
            reserved: [0; 5],
            top_field_order_cnt: reference.top_field_order_cnt,
            bottom_field_order_cnt: reference.bottom_field_order_cnt,
            flags: DPB_ENTRY_FLAG_VALID | DPB_ENTRY_FLAG_ACTIVE | if reference.long_term.is_some() { DPB_ENTRY_FLAG_LONG_TERM } else { 0 },
        };
    }
    params
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::h264::tests::{pps, slice, sps};
    use crate::h264::Tracker;

    #[test]
    fn controls_carry_the_parsed_stream() {
        let mut tracker = Tracker::new();
        tracker.parameter_set(&sps(4)).unwrap();
        tracker.parameter_set(&pps()).unwrap();
        let sequence = sps_control(&tracker.sps[&0]);
        assert_eq!((sequence.profile_idc, sequence.level_idc, sequence.pic_width_in_mbs_minus1, sequence.pic_height_in_map_units_minus1), (100, 42, 119, 67));
        assert_eq!(sequence.flags, SPS_FLAG_FRAME_MBS_ONLY | SPS_FLAG_DIRECT_8X8_INFERENCE);
        let (picture, matrix) = pps_controls(&tracker.pps[&0], &tracker.sps[&0]);
        assert_eq!(picture.flags, PPS_FLAG_ENTROPY_CODING_MODE | PPS_FLAG_WEIGHTED_PRED | PPS_FLAG_DEBLOCKING_FILTER_CONTROL_PRESENT | PPS_FLAG_TRANSFORM_8X8_MODE);
        assert_eq!((matrix.scaling_list_4x4[5][15], matrix.scaling_list_8x8[1][0]), (16, 16));

        tracker.begin(&slice(true, true, SliceType::I, 0, 0, &[]), 1000).unwrap();
        tracker.decoded();
        let picture = tracker.begin(&slice(false, true, SliceType::P, 1, 4, &[]), 2000).unwrap();
        let params = decode_params(&picture);
        assert_eq!((params.frame_num, params.top_field_order_cnt, params.nal_ref_idc, params.flags), (1, 4, 2, DECODE_PARAM_FLAG_PFRAME));
        assert_eq!((params.dpb[0].reference_ts, params.dpb[0].flags, params.dpb[0].fields, params.dpb[1].flags), (1000, 3, 3, 0));
    }
}
