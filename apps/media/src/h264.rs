//! H.264 bitstream work a stateless hardware decoder leaves to its user (ITU-T H.264 clauses
//! 7.3 and 8.2): parameter sets, the first slice header of each picture, picture order counts
//! and reference picture marking. The hardware decodes the slices; it has to be told which
//! earlier pictures are references and where they are.
//!
//! Progressive frames only (YouTube encodes nothing else): field pictures and slice groups are
//! refused with an error rather than decoded wrongly.

use std::collections::HashMap;

pub const NAL_SLICE: u8 = 1;
pub const NAL_IDR: u8 = 5;
pub const NAL_SPS: u8 = 7;
pub const NAL_PPS: u8 = 8;

/// Reads the RBSP of one NAL unit (emulation prevention bytes removed).
struct Bits {
    data: Vec<u8>,
    at: usize,
}

impl Bits {
    /// `nal` without its one-byte header.
    fn new(nal: &[u8]) -> Bits {
        let mut data = Vec::with_capacity(nal.len());
        let mut zeros = 0;
        for &byte in nal {
            if zeros >= 2 && byte == 3 {
                zeros = 0;
                continue;
            }
            zeros = if byte == 0 { zeros + 1 } else { 0 };
            data.push(byte);
        }
        Bits { data, at: 0 }
    }

    fn bit(&mut self) -> Result<u32, String> {
        let byte = *self.data.get(self.at / 8).ok_or("h264: read past the end of a NAL unit")?;
        let bit = (byte >> (7 - self.at % 8)) & 1;
        self.at += 1;
        Ok(bit as u32)
    }

    fn flag(&mut self) -> Result<bool, String> {
        Ok(self.bit()? == 1)
    }

    fn bits(&mut self, count: u32) -> Result<u32, String> {
        let mut value = 0;
        for _ in 0..count {
            value = value << 1 | self.bit()?;
        }
        Ok(value)
    }

    fn ue(&mut self) -> Result<u32, String> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return Err("h264: Exp-Golomb code too long".to_string());
            }
        }
        Ok(((1u64 << zeros) - 1 + self.bits(zeros)? as u64) as u32)
    }

    fn se(&mut self) -> Result<i32, String> {
        let code = self.ue()? as i64;
        Ok(if code & 1 == 1 { (code + 1) / 2 } else { -(code / 2) } as i32)
    }

    /// Whether syntax elements follow before the trailing stop bit.
    fn more_data(&self) -> bool {
        let Some(last) = self.data.iter().rposition(|&b| b != 0) else { return false };
        let stop = last * 8 + 7 - self.data[last].trailing_zeros() as usize;
        self.at < stop
    }
}

/// Six 4x4 and six 8x8 scaling lists in raster order, as V4L2 wants them.
#[derive(Debug, Clone, PartialEq)]
pub struct ScalingLists {
    pub lists4: [[u8; 16]; 6],
    pub lists8: [[u8; 64]; 6],
}

impl ScalingLists {
    fn flat() -> ScalingLists {
        ScalingLists { lists4: [[16; 16]; 6], lists8: [[16; 64]; 6] }
    }
}

const ZIGZAG4: [u8; 16] = [0, 1, 4, 8, 5, 2, 3, 6, 9, 12, 13, 10, 7, 11, 14, 15];
#[rustfmt::skip]
const ZIGZAG8: [u8; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20, 13, 6, 7, 14, 21, 28,
    35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59, 52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];
// Table 7-3 and 7-4, in zigzag order.
const DEFAULT4_INTRA: [u8; 16] = [6, 13, 13, 20, 20, 20, 28, 28, 28, 28, 32, 32, 32, 37, 37, 42];
const DEFAULT4_INTER: [u8; 16] = [10, 14, 14, 20, 20, 20, 24, 24, 24, 24, 27, 27, 27, 30, 30, 34];
#[rustfmt::skip]
const DEFAULT8_INTRA: [u8; 64] = [
    6, 10, 10, 13, 11, 13, 16, 16, 16, 16, 18, 18, 18, 18, 18, 23, 23, 23, 23, 23, 23, 25, 25, 25, 25, 25, 25, 25, 27, 27, 27, 27,
    27, 27, 27, 27, 29, 29, 29, 29, 29, 29, 29, 31, 31, 31, 31, 31, 31, 33, 33, 33, 33, 33, 36, 36, 36, 36, 38, 38, 38, 40, 40, 42,
];
#[rustfmt::skip]
const DEFAULT8_INTER: [u8; 64] = [
    9, 13, 13, 15, 13, 15, 17, 17, 17, 17, 19, 19, 19, 19, 19, 21, 21, 21, 21, 21, 21, 22, 22, 22, 22, 22, 22, 22, 24, 24, 24, 24,
    24, 24, 24, 24, 25, 25, 25, 25, 25, 25, 25, 27, 27, 27, 27, 27, 27, 28, 28, 28, 28, 28, 30, 30, 30, 30, 32, 32, 32, 33, 33, 35,
];

fn raster<const N: usize>(zigzag: &[u8; N], scan: &[u8; N]) -> [u8; N] {
    let mut out = [0; N];
    for (i, &value) in zigzag.iter().enumerate() {
        out[scan[i] as usize] = value;
    }
    out
}

/// One `scaling_list()`: None = "use the default list".
fn read_list<const N: usize>(bits: &mut Bits) -> Result<Option<[u8; N]>, String> {
    let mut list = [0u8; N];
    let (mut last, mut next) = (8i32, 8i32);
    for (i, slot) in list.iter_mut().enumerate() {
        if next != 0 {
            next = (last + bits.se()? + 256) % 256;
            if i == 0 && next == 0 {
                return Ok(None);
            }
        }
        *slot = if next == 0 { last } else { next } as u8;
        last = *slot as i32;
    }
    Ok(Some(list))
}

/// `seq_scaling_matrix` / `pic_scaling_matrix` with the fall-back rules of Table 7-2:
/// `fallback` None is rule A (the defaults), Some is rule B (the sequence's lists).
fn read_matrix(bits: &mut Bits, count8: usize, fallback: Option<&ScalingLists>) -> Result<ScalingLists, String> {
    let mut out = ScalingLists::flat();
    for i in 0..6 {
        let default = raster(if i < 3 { &DEFAULT4_INTRA } else { &DEFAULT4_INTER }, &ZIGZAG4);
        let inherited = match (i, fallback) {
            (0 | 3, None) => default,
            (0 | 3, Some(seq)) => seq.lists4[i],
            _ => out.lists4[i - 1],
        };
        out.lists4[i] = if bits.flag()? { read_list::<16>(bits)?.map_or(default, |l| raster(&l, &ZIGZAG4)) } else { inherited };
    }
    for i in 0..6 {
        let default = raster(if i % 2 == 0 { &DEFAULT8_INTRA } else { &DEFAULT8_INTER }, &ZIGZAG8);
        let inherited = match (i, fallback) {
            (0 | 1, None) => default,
            (0 | 1, Some(seq)) => seq.lists8[i],
            _ => out.lists8[i - 2],
        };
        let present = i < count8 && bits.flag()?;
        out.lists8[i] = if present { read_list::<64>(bits)?.map_or(default, |l| raster(&l, &ZIGZAG8)) } else { inherited };
    }
    Ok(out)
}

#[derive(Debug, Clone, PartialEq)]
pub struct Sps {
    pub profile_idc: u8,
    pub constraint_set_flags: u8,
    pub level_idc: u8,
    pub id: u8,
    pub chroma_format_idc: u8,
    pub separate_colour_plane: bool,
    pub bit_depth_luma_minus8: u8,
    pub bit_depth_chroma_minus8: u8,
    pub qpprime_y_zero_transform_bypass: bool,
    pub log2_max_frame_num_minus4: u8,
    pub pic_order_cnt_type: u8,
    pub log2_max_pic_order_cnt_lsb_minus4: u8,
    pub delta_pic_order_always_zero: bool,
    pub offset_for_non_ref_pic: i32,
    pub offset_for_top_to_bottom_field: i32,
    pub offset_for_ref_frame: Vec<i32>,
    pub max_num_ref_frames: u8,
    pub gaps_in_frame_num_allowed: bool,
    pub pic_width_in_mbs_minus1: u16,
    pub pic_height_in_map_units_minus1: u16,
    pub frame_mbs_only: bool,
    pub mb_adaptive_frame_field: bool,
    pub direct_8x8_inference: bool,
    /// Cropping in luma samples: left, right, top, bottom.
    pub crop: [u32; 4],
    pub scaling: Option<ScalingLists>,
}

impl Sps {
    /// Coded size (multiples of 16), which is what buffers are sized by.
    pub fn coded_size(&self) -> (u32, u32) {
        let height_units = if self.frame_mbs_only { 1 } else { 2 };
        ((self.pic_width_in_mbs_minus1 as u32 + 1) * 16, (self.pic_height_in_map_units_minus1 as u32 + 1) * 16 * height_units)
    }

    /// The visible size after cropping.
    pub fn display_size(&self) -> (u32, u32) {
        let (width, height) = self.coded_size();
        (width.saturating_sub(self.crop[0] + self.crop[1]), height.saturating_sub(self.crop[2] + self.crop[3]))
    }

    fn chroma_array_type(&self) -> u8 {
        if self.separate_colour_plane { 0 } else { self.chroma_format_idc }
    }
}

/// `nal` is the whole NAL unit, header byte included (as for every parser here).
pub fn parse_sps(nal: &[u8]) -> Result<Sps, String> {
    let mut bits = Bits::new(nal.get(1..).ok_or("h264: empty SPS")?);
    let profile_idc = bits.bits(8)? as u8;
    let constraint_set_flags = (bits.bits(8)? as u8).reverse_bits() & 0x3f;
    let level_idc = bits.bits(8)? as u8;
    let id = bits.ue()?;
    if id > 31 {
        return Err("h264: SPS id out of range".to_string());
    }
    let (mut chroma_format_idc, mut separate_colour_plane, mut depth_luma, mut depth_chroma, mut bypass, mut scaling) = (1, false, 0, 0, false, None);
    if matches!(profile_idc, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
        chroma_format_idc = bits.ue()?;
        if chroma_format_idc > 3 {
            return Err("h264: bad chroma format".to_string());
        }
        if chroma_format_idc == 3 {
            separate_colour_plane = bits.flag()?;
        }
        depth_luma = bits.ue()?;
        depth_chroma = bits.ue()?;
        if depth_luma > 6 || depth_chroma > 6 {
            return Err("h264: bad bit depth".to_string());
        }
        bypass = bits.flag()?;
        if bits.flag()? {
            scaling = Some(read_matrix(&mut bits, if chroma_format_idc == 3 { 6 } else { 2 }, None)?);
        }
    }
    let log2_max_frame_num_minus4 = bits.ue()?;
    let pic_order_cnt_type = bits.ue()?;
    if log2_max_frame_num_minus4 > 12 || pic_order_cnt_type > 2 {
        return Err("h264: bad SPS".to_string());
    }
    let (mut log2_poc, mut always_zero, mut offset_non_ref, mut offset_top_bottom, mut offsets) = (0, false, 0, 0, Vec::new());
    if pic_order_cnt_type == 0 {
        log2_poc = bits.ue()?;
        if log2_poc > 12 {
            return Err("h264: bad SPS".to_string());
        }
    } else if pic_order_cnt_type == 1 {
        always_zero = bits.flag()?;
        offset_non_ref = bits.se()?;
        offset_top_bottom = bits.se()?;
        let count = bits.ue()?;
        if count > 255 {
            return Err("h264: bad SPS".to_string());
        }
        offsets = (0..count).map(|_| bits.se()).collect::<Result<_, _>>()?;
    }
    let max_num_ref_frames = bits.ue()?;
    let gaps = bits.flag()?;
    let width = bits.ue()?;
    let height = bits.ue()?;
    if max_num_ref_frames > 16 || width > 1023 || height > 1023 {
        return Err("h264: bad SPS".to_string());
    }
    let frame_mbs_only = bits.flag()?;
    let mbaff = if frame_mbs_only { false } else { bits.flag()? };
    let direct_8x8_inference = bits.flag()?;
    let mut crop = [0; 4];
    if bits.flag()? {
        let (unit_x, unit_y) = match chroma_format_idc {
            0 | 3 => (1, 1),
            1 => (2, 2),
            _ => (2, 1),
        };
        let unit_y = unit_y * if frame_mbs_only { 1 } else { 2 };
        crop = [bits.ue()? * unit_x, bits.ue()? * unit_x, bits.ue()? * unit_y, bits.ue()? * unit_y];
    }
    // The VUI that may follow only matters to a decoder that orders output itself.
    Ok(Sps {
        profile_idc,
        constraint_set_flags,
        level_idc,
        id: id as u8,
        chroma_format_idc: chroma_format_idc as u8,
        separate_colour_plane,
        bit_depth_luma_minus8: depth_luma as u8,
        bit_depth_chroma_minus8: depth_chroma as u8,
        qpprime_y_zero_transform_bypass: bypass,
        log2_max_frame_num_minus4: log2_max_frame_num_minus4 as u8,
        pic_order_cnt_type: pic_order_cnt_type as u8,
        log2_max_pic_order_cnt_lsb_minus4: log2_poc as u8,
        delta_pic_order_always_zero: always_zero,
        offset_for_non_ref_pic: offset_non_ref,
        offset_for_top_to_bottom_field: offset_top_bottom,
        offset_for_ref_frame: offsets,
        max_num_ref_frames: max_num_ref_frames as u8,
        gaps_in_frame_num_allowed: gaps,
        pic_width_in_mbs_minus1: width as u16,
        pic_height_in_map_units_minus1: height as u16,
        frame_mbs_only,
        mb_adaptive_frame_field: mbaff,
        direct_8x8_inference,
        crop,
        scaling,
    })
}

#[derive(Debug, Clone, PartialEq)]
pub struct Pps {
    pub id: u8,
    pub sps_id: u8,
    pub entropy_coding_mode: bool,
    pub bottom_field_pic_order_in_frame_present: bool,
    pub num_ref_idx_l0_default_active_minus1: u8,
    pub num_ref_idx_l1_default_active_minus1: u8,
    pub weighted_pred: bool,
    pub weighted_bipred_idc: u8,
    pub pic_init_qp_minus26: i8,
    pub pic_init_qs_minus26: i8,
    pub chroma_qp_index_offset: i8,
    pub second_chroma_qp_index_offset: i8,
    pub deblocking_filter_control_present: bool,
    pub constrained_intra_pred: bool,
    pub redundant_pic_cnt_present: bool,
    pub transform_8x8_mode: bool,
    pub scaling: Option<ScalingLists>,
}

/// The PPS depends on its SPS (chroma format, sequence scaling lists), found by id in `sps`.
pub fn parse_pps(nal: &[u8], sps: &HashMap<u8, Sps>) -> Result<Pps, String> {
    let mut bits = Bits::new(nal.get(1..).ok_or("h264: empty PPS")?);
    let id = bits.ue()?;
    let sps_id = bits.ue()?;
    if id > 255 || sps_id > 31 {
        return Err("h264: PPS id out of range".to_string());
    }
    let sequence = sps.get(&(sps_id as u8)).ok_or("h264: PPS for an unknown SPS")?;
    let entropy_coding_mode = bits.flag()?;
    let bottom_field_pic_order_in_frame_present = bits.flag()?;
    if bits.ue()? != 0 {
        return Err("h264: slice groups (FMO) are not supported".to_string());
    }
    let l0 = bits.ue()?;
    let l1 = bits.ue()?;
    if l0 > 31 || l1 > 31 {
        return Err("h264: bad PPS".to_string());
    }
    let weighted_pred = bits.flag()?;
    let weighted_bipred_idc = bits.bits(2)? as u8;
    let pic_init_qp_minus26 = bits.se()? as i8;
    let pic_init_qs_minus26 = bits.se()? as i8;
    let chroma_qp_index_offset = bits.se()? as i8;
    let deblocking_filter_control_present = bits.flag()?;
    let constrained_intra_pred = bits.flag()?;
    let redundant_pic_cnt_present = bits.flag()?;
    let (mut transform_8x8_mode, mut scaling, mut second) = (false, None, chroma_qp_index_offset);
    if bits.more_data() {
        transform_8x8_mode = bits.flag()?;
        if bits.flag()? {
            let count8 = if !transform_8x8_mode { 0 } else if sequence.chroma_format_idc == 3 { 6 } else { 2 };
            scaling = Some(read_matrix(&mut bits, count8, sequence.scaling.as_ref())?);
        }
        second = bits.se()? as i8;
    }
    Ok(Pps {
        id: id as u8,
        sps_id: sps_id as u8,
        entropy_coding_mode,
        bottom_field_pic_order_in_frame_present,
        num_ref_idx_l0_default_active_minus1: l0 as u8,
        num_ref_idx_l1_default_active_minus1: l1 as u8,
        weighted_pred,
        weighted_bipred_idc,
        pic_init_qp_minus26,
        pic_init_qs_minus26,
        chroma_qp_index_offset,
        second_chroma_qp_index_offset: second,
        deblocking_filter_control_present,
        constrained_intra_pred,
        redundant_pic_cnt_present,
        transform_8x8_mode,
        scaling,
    })
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SliceType {
    P,
    B,
    I,
}

/// Memory management control operations (7.4.3.3).
#[derive(Debug, Clone, Copy, PartialEq)]
enum Mmco {
    ForgetShort { difference_minus1: u32 },
    ForgetLong { long_term_pic_num: u32 },
    ShortToLong { difference_minus1: u32, index: u32 },
    MaxLongIndex { plus1: u32 },
    ForgetAll,
    CurrentToLong { index: u32 },
}

#[derive(Debug, Clone, PartialEq)]
struct SliceHeader {
    slice_type: SliceType,
    pps_id: u8,
    frame_num: u16,
    idr_pic_id: u16,
    pic_order_cnt_lsb: u16,
    delta_pic_order_cnt_bottom: i32,
    delta_pic_order_cnt: [i32; 2],
    pic_order_cnt_bit_size: u32,
    dec_ref_pic_marking_bit_size: u32,
    long_term_reference: bool,
    /// None: sliding window.
    mmco: Option<Vec<Mmco>>,
}

fn parse_slice_header(nal: &[u8], sps: &HashMap<u8, Sps>, pps: &HashMap<u8, Pps>) -> Result<SliceHeader, String> {
    let header = *nal.first().ok_or("h264: empty slice")?;
    let (nal_ref_idc, nal_type) = (header >> 5 & 3, header & 0x1f);
    let idr = nal_type == NAL_IDR;
    // The header is short; unescaping the whole slice would copy megabytes per second.
    let mut bits = Bits::new(&nal[1..nal.len().min(1 + 512)]);
    bits.ue()?; // first_mb_in_slice
    let slice_type = match bits.ue()? % 5 {
        0 | 3 => SliceType::P,
        1 => SliceType::B,
        _ => SliceType::I,
    };
    let pps_id = bits.ue()?;
    let picture = pps.get(&(pps_id.min(255) as u8)).ok_or("h264: slice for an unknown PPS")?;
    let sequence = sps.get(&picture.sps_id).ok_or("h264: slice for an unknown SPS")?;
    if sequence.separate_colour_plane {
        bits.bits(2)?;
    }
    let frame_num = bits.bits(sequence.log2_max_frame_num_minus4 as u32 + 4)? as u16;
    if !sequence.frame_mbs_only && bits.flag()? {
        return Err("h264: field pictures (interlaced video) are not supported".to_string());
    }
    let idr_pic_id = if idr { bits.ue()? as u16 } else { 0 };
    let poc_start = bits.at;
    let (mut lsb, mut delta_bottom, mut delta) = (0, 0, [0; 2]);
    if sequence.pic_order_cnt_type == 0 {
        lsb = bits.bits(sequence.log2_max_pic_order_cnt_lsb_minus4 as u32 + 4)? as u16;
        if picture.bottom_field_pic_order_in_frame_present {
            delta_bottom = bits.se()?;
        }
    } else if sequence.pic_order_cnt_type == 1 && !sequence.delta_pic_order_always_zero {
        delta[0] = bits.se()?;
        if picture.bottom_field_pic_order_in_frame_present {
            delta[1] = bits.se()?;
        }
    }
    let pic_order_cnt_bit_size = (bits.at - poc_start) as u32;
    if picture.redundant_pic_cnt_present {
        bits.ue()?;
    }
    if slice_type == SliceType::B {
        bits.flag()?; // direct_spatial_mv_pred
    }
    let mut active = [picture.num_ref_idx_l0_default_active_minus1 as u32, picture.num_ref_idx_l1_default_active_minus1 as u32];
    let lists = match slice_type {
        SliceType::I => 0,
        SliceType::P => 1,
        SliceType::B => 2,
    };
    if lists > 0 && bits.flag()? {
        for slot in active.iter_mut().take(lists) {
            *slot = bits.ue()?;
        }
    }
    if active[0] > 31 || active[1] > 31 {
        return Err("h264: bad slice header".to_string());
    }
    for _ in 0..lists {
        // ref_pic_list_modification
        if bits.flag()? {
            loop {
                match bits.ue()? {
                    0..=2 => bits.ue()?,
                    3 => break,
                    _ => return Err("h264: bad reference list modification".to_string()),
                };
            }
        }
    }
    let weighted = (picture.weighted_pred && slice_type == SliceType::P) || (picture.weighted_bipred_idc == 1 && slice_type == SliceType::B);
    if weighted {
        // pred_weight_table
        let chroma = sequence.chroma_array_type() != 0;
        bits.ue()?;
        if chroma {
            bits.ue()?;
        }
        for &count in active.iter().take(lists) {
            for _ in 0..=count {
                if bits.flag()? {
                    bits.se()?;
                    bits.se()?;
                }
                if chroma && bits.flag()? {
                    for _ in 0..4 {
                        bits.se()?;
                    }
                }
            }
        }
    }
    let marking_start = bits.at;
    let (mut long_term_reference, mut mmco) = (false, None);
    if nal_ref_idc != 0 {
        if idr {
            bits.flag()?; // no_output_of_prior_pics
            long_term_reference = bits.flag()?;
        } else if bits.flag()? {
            let mut operations = Vec::new();
            loop {
                let operation = match bits.ue()? {
                    0 => break,
                    1 => Mmco::ForgetShort { difference_minus1: bits.ue()? },
                    2 => Mmco::ForgetLong { long_term_pic_num: bits.ue()? },
                    3 => Mmco::ShortToLong { difference_minus1: bits.ue()?, index: bits.ue()? },
                    4 => Mmco::MaxLongIndex { plus1: bits.ue()? },
                    5 => Mmco::ForgetAll,
                    6 => Mmco::CurrentToLong { index: bits.ue()? },
                    _ => return Err("h264: bad memory management operation".to_string()),
                };
                operations.push(operation);
                if operations.len() > 66 {
                    return Err("h264: too many memory management operations".to_string());
                }
            }
            mmco = Some(operations);
        }
    }
    Ok(SliceHeader {
        slice_type,
        pps_id: picture.id,
        frame_num,
        idr_pic_id,
        pic_order_cnt_lsb: lsb,
        delta_pic_order_cnt_bottom: delta_bottom,
        delta_pic_order_cnt: delta,
        pic_order_cnt_bit_size,
        dec_ref_pic_marking_bit_size: (bits.at - marking_start) as u32,
        long_term_reference,
        mmco,
    })
}

/// A decoded picture kept as a reference.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Reference {
    /// The caller's name for the picture (the timestamp its buffer carries).
    pub key: u64,
    pub frame_num: u16,
    /// Some(LongTermFrameIdx) for a long-term reference.
    pub long_term: Option<u32>,
    pub top_field_order_cnt: i32,
    pub bottom_field_order_cnt: i32,
}

/// Everything the hardware needs to decode one picture, besides the slice data.
#[derive(Debug, Clone, PartialEq)]
pub struct Picture {
    pub sps_id: u8,
    pub pps_id: u8,
    pub idr: bool,
    pub slice_type: SliceType,
    pub nal_ref_idc: u8,
    pub frame_num: u16,
    pub idr_pic_id: u16,
    pub pic_order_cnt_lsb: u16,
    pub delta_pic_order_cnt_bottom: i32,
    pub delta_pic_order_cnt: [i32; 2],
    pub pic_order_cnt_bit_size: u32,
    pub dec_ref_pic_marking_bit_size: u32,
    pub top_field_order_cnt: i32,
    pub bottom_field_order_cnt: i32,
    /// The reference pictures as they are while this picture decodes.
    pub references: Vec<Reference>,
}

impl Picture {
    /// Output order within one coded video sequence (from one IDR to the next).
    pub fn order(&self) -> i32 {
        self.top_field_order_cnt.min(self.bottom_field_order_cnt)
    }
}

/// Follows a stream picture by picture: parameter sets, order counts and the reference set.
#[derive(Default)]
pub struct Tracker {
    pub sps: HashMap<u8, Sps>,
    pub pps: HashMap<u8, Pps>,
    references: Vec<Reference>,
    prev_poc_msb: i32,
    prev_poc_lsb: i32,
    prev_frame_num: u32,
    prev_frame_num_offset: u32,
    prev_forgot_all: bool,
    /// The marking that `decoded` applies for the picture `begin` returned.
    pending: Option<(SliceHeader, Reference, bool)>,
}

impl Tracker {
    pub fn new() -> Tracker {
        Tracker::default()
    }

    /// Feed a parameter-set NAL unit (from `avcC` or in the stream). Other types are ignored.
    pub fn parameter_set(&mut self, nal: &[u8]) -> Result<(), String> {
        match nal.first().map(|b| b & 0x1f) {
            Some(NAL_SPS) => {
                let sps = parse_sps(nal)?;
                self.sps.insert(sps.id, sps);
            }
            Some(NAL_PPS) => {
                let pps = parse_pps(nal, &self.sps)?;
                self.pps.insert(pps.id, pps);
            }
            _ => {}
        }
        Ok(())
    }

    /// Start a picture from its first slice NAL unit. `key` names the picture's buffer.
    /// Call `decoded` once the hardware has taken it, before the next `begin`.
    pub fn begin(&mut self, slice: &[u8], key: u64) -> Result<Picture, String> {
        let header_byte = *slice.first().ok_or("h264: empty slice")?;
        let (nal_ref_idc, nal_type) = (header_byte >> 5 & 3, header_byte & 0x1f);
        if nal_type != NAL_SLICE && nal_type != NAL_IDR {
            return Err(format!("h264: NAL unit type {nal_type} is not a slice"));
        }
        let idr = nal_type == NAL_IDR;
        let header = parse_slice_header(slice, &self.sps, &self.pps)?;
        let pps = &self.pps[&header.pps_id];
        let sps = &self.sps[&pps.sps_id];
        let max_frame_num = 1u32 << (sps.log2_max_frame_num_minus4 + 4);
        let frame_num = header.frame_num as u32;
        if idr {
            self.references.clear();
        }

        let frame_num_offset = if idr {
            0
        } else {
            let previous = if self.prev_forgot_all { 0 } else { self.prev_frame_num_offset };
            if self.prev_frame_num > frame_num { previous + max_frame_num } else { previous }
        };
        let (top, bottom) = match sps.pic_order_cnt_type {
            0 => {
                let max_lsb = 1i32 << (sps.log2_max_pic_order_cnt_lsb_minus4 + 4);
                let (prev_msb, prev_lsb) = if idr { (0, 0) } else { (self.prev_poc_msb, self.prev_poc_lsb) };
                let lsb = header.pic_order_cnt_lsb as i32;
                let msb = if lsb < prev_lsb && prev_lsb - lsb >= max_lsb / 2 {
                    prev_msb + max_lsb
                } else if lsb > prev_lsb && lsb - prev_lsb > max_lsb / 2 {
                    prev_msb - max_lsb
                } else {
                    prev_msb
                };
                if nal_ref_idc != 0 {
                    self.prev_poc_msb = msb;
                    self.prev_poc_lsb = lsb;
                }
                (msb + lsb, msb + lsb + header.delta_pic_order_cnt_bottom)
            }
            1 => {
                let cycle = sps.offset_for_ref_frame.len() as i64;
                let mut abs_frame_num = if cycle != 0 { frame_num_offset as i64 + frame_num as i64 } else { 0 };
                if nal_ref_idc == 0 && abs_frame_num > 0 {
                    abs_frame_num -= 1;
                }
                let mut expected = 0i64;
                if abs_frame_num > 0 {
                    let total: i64 = sps.offset_for_ref_frame.iter().map(|&o| o as i64).sum();
                    let cycles = (abs_frame_num - 1) / cycle;
                    let position = ((abs_frame_num - 1) % cycle) as usize;
                    expected = cycles * total + sps.offset_for_ref_frame[..=position].iter().map(|&o| o as i64).sum::<i64>();
                }
                if nal_ref_idc == 0 {
                    expected += sps.offset_for_non_ref_pic as i64;
                }
                let top = expected as i32 + header.delta_pic_order_cnt[0];
                (top, top + sps.offset_for_top_to_bottom_field + header.delta_pic_order_cnt[1])
            }
            _ => {
                let order = if idr { 0 } else { 2 * (frame_num_offset + frame_num) as i32 - (nal_ref_idc == 0) as i32 };
                (order, order)
            }
        };
        self.prev_frame_num = frame_num;
        self.prev_frame_num_offset = frame_num_offset;
        self.prev_forgot_all = false;

        let picture = Picture {
            sps_id: sps.id,
            pps_id: pps.id,
            idr,
            slice_type: header.slice_type,
            nal_ref_idc,
            frame_num: header.frame_num,
            idr_pic_id: header.idr_pic_id,
            pic_order_cnt_lsb: header.pic_order_cnt_lsb,
            delta_pic_order_cnt_bottom: header.delta_pic_order_cnt_bottom,
            delta_pic_order_cnt: header.delta_pic_order_cnt,
            pic_order_cnt_bit_size: header.pic_order_cnt_bit_size,
            dec_ref_pic_marking_bit_size: header.dec_ref_pic_marking_bit_size,
            top_field_order_cnt: top,
            bottom_field_order_cnt: bottom,
            references: self.references.clone(),
        };
        let current = Reference { key, frame_num: header.frame_num, long_term: None, top_field_order_cnt: top, bottom_field_order_cnt: bottom };
        self.pending = Some((header, current, nal_ref_idc != 0));
        Ok(picture)
    }

    /// Apply the reference marking of the picture `begin` returned (8.2.5).
    pub fn decoded(&mut self) {
        let Some((header, mut current, is_reference)) = self.pending.take() else { return };
        if !is_reference {
            return;
        }
        let pps = &self.pps[&header.pps_id];
        let sps = &self.sps[&pps.sps_id];
        let max_frame_num = 1i64 << (sps.log2_max_frame_num_minus4 + 4);
        let current_num = current.frame_num as i64;
        // PicNum of a short-term frame, relative to the current picture (8.2.4.1).
        let pic_num = |r: &Reference| if r.frame_num as i64 > current_num { r.frame_num as i64 - max_frame_num } else { r.frame_num as i64 };

        if header.long_term_reference {
            current.long_term = Some(0);
        }
        match &header.mmco {
            Some(operations) => {
                for operation in operations {
                    match *operation {
                        Mmco::ForgetShort { difference_minus1 } => {
                            let target = current_num - (difference_minus1 as i64 + 1);
                            self.references.retain(|r| r.long_term.is_some() || pic_num(r) != target);
                        }
                        Mmco::ForgetLong { long_term_pic_num } => self.references.retain(|r| r.long_term != Some(long_term_pic_num)),
                        Mmco::ShortToLong { difference_minus1, index } => {
                            let target = current_num - (difference_minus1 as i64 + 1);
                            self.references.retain(|r| r.long_term != Some(index));
                            if let Some(r) = self.references.iter_mut().find(|r| r.long_term.is_none() && pic_num(r) == target) {
                                r.long_term = Some(index);
                            }
                        }
                        Mmco::MaxLongIndex { plus1 } => self.references.retain(|r| r.long_term.is_none_or(|index| index < plus1)),
                        Mmco::ForgetAll => {
                            self.references.clear();
                            // The picture becomes the start of a new numbering (7.4.3, 8.2.1).
                            let order = current.top_field_order_cnt.min(current.bottom_field_order_cnt);
                            current.top_field_order_cnt -= order;
                            current.bottom_field_order_cnt -= order;
                            current.frame_num = 0;
                            self.prev_frame_num = 0;
                            self.prev_forgot_all = true;
                            self.prev_poc_msb = 0;
                            self.prev_poc_lsb = current.top_field_order_cnt;
                        }
                        Mmco::CurrentToLong { index } => {
                            self.references.retain(|r| r.long_term != Some(index));
                            current.long_term = Some(index);
                        }
                    }
                }
            }
            None => {
                // Sliding window: the oldest short-term picture makes room.
                let limit = (sps.max_num_ref_frames as usize).max(1);
                while self.references.len() >= limit {
                    let oldest = self.references.iter().enumerate().filter(|(_, r)| r.long_term.is_none()).min_by_key(|(_, r)| pic_num(r)).map(|(i, _)| i);
                    match oldest {
                        Some(index) => self.references.remove(index),
                        None => break,
                    };
                }
            }
        }
        self.references.push(current);
        // A broken stream must not grow the set beyond what the hardware interface holds.
        while self.references.len() > 16 {
            self.references.remove(0);
        }
    }

    /// The pictures that later ones may still refer to: their buffers must not be reused.
    pub fn references(&self) -> &[Reference] {
        &self.references
    }

    /// Forget all pictures (after a seek); parameter sets stay.
    pub fn reset(&mut self) {
        self.references.clear();
        self.pending = None;
        (self.prev_poc_msb, self.prev_poc_lsb, self.prev_frame_num, self.prev_frame_num_offset, self.prev_forgot_all) = (0, 0, 0, 0, false);
    }
}

/// The NAL units of one MP4 sample (each prefixed by a big-endian length of `length_size` bytes).
pub fn nal_units(sample: &[u8], length_size: usize) -> impl Iterator<Item = Result<&[u8], String>> {
    let mut at = 0;
    std::iter::from_fn(move || {
        if at >= sample.len() {
            return None;
        }
        let Some(prefix) = sample.get(at..at + length_size) else {
            at = sample.len();
            return Some(Err("h264: truncated NAL length".to_string()));
        };
        let length = prefix.iter().fold(0usize, |n, &b| n << 8 | b as usize);
        let start = at + length_size;
        at = start + length;
        match sample.get(start..start + length) {
            Some(nal) if length > 0 => Some(Ok(nal)),
            _ => {
                at = sample.len();
                Some(Err("h264: NAL unit outside its sample".to_string()))
            }
        }
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Writes syntax elements; `nal` adds the header byte, the stop bit and emulation prevention.
    #[derive(Default)]
    pub struct Writer {
        bits: Vec<bool>,
    }

    impl Writer {
        pub fn u(mut self, count: u32, value: u32) -> Self {
            for i in (0..count).rev() {
                self.bits.push(value >> i & 1 == 1);
            }
            self
        }
        pub fn ue(self, value: u32) -> Self {
            let code = value as u64 + 1;
            let length = 64 - code.leading_zeros();
            self.u(length - 1, 0).u(length, code as u32)
        }
        pub fn se(self, value: i32) -> Self {
            self.ue(if value > 0 { 2 * value as u32 - 1 } else { 2 * (-value) as u32 })
        }
        pub fn nal(mut self, header: u8) -> Vec<u8> {
            self.bits.push(true);
            while self.bits.len() % 8 != 0 {
                self.bits.push(false);
            }
            let mut out = vec![header];
            let mut zeros = 0;
            for chunk in self.bits.chunks(8) {
                let byte = chunk.iter().fold(0u8, |b, &bit| b << 1 | bit as u8);
                if zeros >= 2 && byte <= 3 {
                    out.push(3);
                    zeros = 0;
                }
                zeros = if byte == 0 { zeros + 1 } else { 0 };
                out.push(byte);
            }
            out
        }
    }

    /// High profile, 1920x1080 (cropped from 1088), POC type 0 with 6-bit lsb, 4-bit frame_num.
    pub fn sps(max_refs: u32) -> Vec<u8> {
        Writer::default()
            .u(8, 100).u(8, 0).u(8, 42).ue(0) // profile, constraints, level 4.2, id
            .ue(1).ue(0).ue(0).u(1, 0).u(1, 0) // 4:2:0, 8 bit, no bypass, no scaling matrix
            .ue(0).ue(0).ue(2) // log2_max_frame_num 4, poc type 0, log2_max_poc_lsb 6
            .ue(max_refs).u(1, 0).ue(119).ue(67) // refs, no gaps, 120x68 macroblocks
            .u(1, 1).u(1, 1) // frame_mbs_only, direct_8x8
            .u(1, 1).ue(0).ue(0).ue(0).ue(4) // crop 8 lines at the bottom
            .u(1, 0) // no VUI
            .nal(0x67)
    }

    pub fn pps() -> Vec<u8> {
        Writer::default()
            .ue(0).ue(0).u(1, 1).u(1, 0).ue(0) // ids, CABAC, no bottom-field POC, one slice group
            .ue(2).ue(0).u(1, 1).u(2, 2) // 3 default L0 refs, weighted P prediction, implicit B
            .se(0).se(0).se(-2).u(1, 1).u(1, 0).u(1, 0) // qp, qs, chroma offset, deblocking control
            .u(1, 1).u(1, 0).se(-2) // 8x8 transform, no scaling matrix, second chroma offset
            .nal(0x68)
    }

    /// The start of a slice: enough for the tracker (macroblock data is never read).
    pub fn slice(idr: bool, reference: bool, kind: SliceType, frame_num: u32, poc_lsb: u32, mmco: &[(u32, &[u32])]) -> Vec<u8> {
        let slice_type = match kind { SliceType::P => 5, SliceType::B => 6, SliceType::I => 7 };
        let mut w = Writer::default().ue(0).ue(slice_type).ue(0).u(4, frame_num);
        if idr {
            w = w.ue(7);
        }
        w = w.u(6, poc_lsb);
        if kind == SliceType::B {
            w = w.u(1, 1);
        }
        if kind != SliceType::I {
            w = w.u(1, 1).ue(0); // override: one L0 reference
            if kind == SliceType::B {
                w = w.ue(0);
            }
            w = w.u(1, 1).ue(0).ue(0).ue(3); // L0 modification: one entry
            if kind == SliceType::B {
                w = w.u(1, 0);
            }
        }
        if kind == SliceType::P {
            w = w.ue(5).ue(5).u(1, 1).se(3).se(-1).u(1, 0); // weights for the one L0 reference
        }
        if reference {
            if idr {
                w = w.u(1, 0).u(1, 0);
            } else if mmco.is_empty() {
                w = w.u(1, 0);
            } else {
                w = w.u(1, 1);
                for (operation, arguments) in mmco {
                    w = w.ue(*operation);
                    for argument in *arguments {
                        w = w.ue(*argument);
                    }
                }
                w = w.ue(0);
            }
        }
        // Stand-in for slice data, including a pattern that needs emulation prevention.
        w.u(8, 0xff).u(8, 0).u(8, 0).u(8, 1).nal(if idr { 0x65 } else if reference { 0x41 } else { 0x01 })
    }

    fn tracker(max_refs: u32) -> Tracker {
        let mut tracker = Tracker::new();
        tracker.parameter_set(&sps(max_refs)).unwrap();
        tracker.parameter_set(&pps()).unwrap();
        tracker
    }

    fn keys(tracker: &Tracker) -> Vec<u64> {
        tracker.references().iter().map(|r| r.key).collect()
    }

    #[test]
    fn exp_golomb_round_trips() {
        let nal = Writer::default().ue(0).ue(1).ue(255).ue(65535).se(0).se(1).se(-1).se(-300).u(1, 1).nal(0);
        let mut bits = Bits::new(&nal[1..]);
        assert_eq!((bits.ue(), bits.ue(), bits.ue(), bits.ue()), (Ok(0), Ok(1), Ok(255), Ok(65535)));
        assert_eq!((bits.se(), bits.se(), bits.se(), bits.se()), (Ok(0), Ok(1), Ok(-1), Ok(-300)));
        assert!(bits.more_data());
        assert_eq!(bits.flag(), Ok(true));
        assert!(!bits.more_data());
        assert!(bits.bits(32).is_err());
    }

    #[test]
    fn emulation_prevention_is_removed() {
        let bits = Bits::new(&[0, 0, 3, 1, 0, 0, 3, 0, 0, 3]);
        assert_eq!(bits.data, vec![0, 0, 1, 0, 0, 0, 0]);
    }

    #[test]
    fn parses_parameter_sets() {
        let sequence = parse_sps(&sps(4)).unwrap();
        assert_eq!((sequence.profile_idc, sequence.level_idc, sequence.max_num_ref_frames, sequence.chroma_format_idc), (100, 42, 4, 1));
        assert_eq!((sequence.coded_size(), sequence.display_size()), ((1920, 1088), (1920, 1080)));
        assert_eq!((sequence.log2_max_frame_num_minus4, sequence.log2_max_pic_order_cnt_lsb_minus4, sequence.frame_mbs_only), (0, 2, true));
        let tracker = tracker(4);
        let picture = &tracker.pps[&0];
        assert_eq!((picture.entropy_coding_mode, picture.weighted_pred, picture.weighted_bipred_idc, picture.num_ref_idx_l0_default_active_minus1), (true, true, 2, 2));
        assert_eq!((picture.chroma_qp_index_offset, picture.second_chroma_qp_index_offset, picture.transform_8x8_mode), (-2, -2, true));
        assert!(picture.scaling.is_none() && sequence.scaling.is_none());
    }

    #[test]
    fn scaling_lists_follow_the_fallback_rules() {
        // A sequence matrix where only list 0 is sent (all 20s), list 1 asks for the default.
        let mut w = Writer::default().u(8, 100).u(8, 0).u(8, 40).ue(0).ue(1).ue(0).ue(0).u(1, 0).u(1, 1);
        w = w.u(1, 1).se(12); // list 0: first value 8 + 12 = 20 ...
        w = w.se(-20); // ... and "repeat it" (next = 0 after the first entry)
        w = w.u(1, 1).se(-8); // list 1: use the default
        for _ in 2..8 {
            w = w.u(1, 0);
        }
        let nal = w.ue(0).ue(2).ue(1).u(1, 0).ue(119).ue(67).u(1, 1).u(1, 1).u(1, 0).u(1, 0).nal(0x67);
        let lists = parse_sps(&nal).unwrap().scaling.unwrap();
        assert_eq!(lists.lists4[0], [20; 16]);
        assert_eq!(lists.lists4[1], raster(&DEFAULT4_INTRA, &ZIGZAG4));
        assert_eq!(lists.lists4[2], lists.lists4[1]); // not sent: the previous list
        assert_eq!(lists.lists4[3], raster(&DEFAULT4_INTER, &ZIGZAG4)); // not sent: rule A default
        assert_eq!((lists.lists8[0][0], lists.lists8[0][63], lists.lists8[1][0]), (6, 42, 9));
        assert_eq!(lists.lists4[1][..4], [6, 13, 20, 28]); // raster: first row of the intra default
    }

    #[test]
    fn slice_header_fields_and_bit_sizes() {
        let tracker = tracker(4);
        let idr = parse_slice_header(&slice(true, true, SliceType::I, 0, 0, &[]), &tracker.sps, &tracker.pps).unwrap();
        assert_eq!((idr.slice_type, idr.idr_pic_id, idr.pic_order_cnt_bit_size, idr.dec_ref_pic_marking_bit_size), (SliceType::I, 7, 6, 2));
        let p = parse_slice_header(&slice(false, true, SliceType::P, 3, 12, &[]), &tracker.sps, &tracker.pps).unwrap();
        assert_eq!((p.slice_type, p.frame_num, p.pic_order_cnt_lsb, p.dec_ref_pic_marking_bit_size, p.mmco.is_none()), (SliceType::P, 3, 12, 1, true));
        let b = parse_slice_header(&slice(false, false, SliceType::B, 4, 10, &[]), &tracker.sps, &tracker.pps).unwrap();
        assert_eq!((b.slice_type, b.dec_ref_pic_marking_bit_size), (SliceType::B, 0));
        // adaptive flag (1) + ue(1)=3 bits + ue(0)=1 bit + end ue(0)=1 bit
        let marked = parse_slice_header(&slice(false, true, SliceType::P, 3, 12, &[(1, &[0])]), &tracker.sps, &tracker.pps).unwrap();
        assert_eq!((marked.dec_ref_pic_marking_bit_size, marked.mmco), (6, Some(vec![Mmco::ForgetShort { difference_minus1: 0 }])));
    }

    #[test]
    fn sliding_window_keeps_the_newest_references() {
        let mut tracker = tracker(2);
        // Decode order I0 P1 b P2 b P3: display order by POC.
        let frames = [(true, true, SliceType::I, 0, 0), (false, true, SliceType::P, 1, 4), (false, false, SliceType::B, 2, 2), (false, true, SliceType::P, 2, 8), (false, false, SliceType::B, 3, 6), (false, true, SliceType::P, 3, 12)];
        let mut seen = Vec::new();
        for (key, (idr, reference, kind, frame_num, poc)) in frames.into_iter().enumerate() {
            let picture = tracker.begin(&slice(idr, reference, kind, frame_num, poc, &[]), key as u64).unwrap();
            seen.push((picture.order(), picture.references.iter().map(|r| r.key).collect::<Vec<_>>()));
            tracker.decoded();
        }
        assert_eq!(seen, vec![(0, vec![]), (4, vec![0]), (2, vec![0, 1]), (8, vec![0, 1]), (6, vec![1, 3]), (12, vec![1, 3])]);
        assert_eq!(keys(&tracker), vec![3, 5]);
        // The next IDR empties the set before it decodes.
        let picture = tracker.begin(&slice(true, true, SliceType::I, 0, 0, &[]), 9).unwrap();
        assert!(picture.idr && picture.references.is_empty());
    }

    #[test]
    fn order_count_wraps_and_frame_num_wraps() {
        let mut tracker = tracker(1);
        let mut orders = Vec::new();
        // 6-bit lsb wraps at 64, 4-bit frame_num at 16: 40 P frames, two apart.
        for i in 0..40u32 {
            let nal = slice(i == 0, true, if i == 0 { SliceType::I } else { SliceType::P }, i % 16, (i * 2) % 64, &[]);
            orders.push(tracker.begin(&nal, i as u64).unwrap().order());
            tracker.decoded();
        }
        assert_eq!(orders, (0..40).map(|i| i * 2).collect::<Vec<i32>>());
        assert_eq!(keys(&tracker), vec![39]);
    }

    #[test]
    fn memory_management_operations() {
        let mut tracker = tracker(4);
        let step = |tracker: &mut Tracker, key: u64, frame_num: u32, mmco: &[(u32, &[u32])]| {
            let nal = slice(key == 0, true, if key == 0 { SliceType::I } else { SliceType::P }, frame_num, key as u32 * 2, mmco);
            let picture = tracker.begin(&nal, key).unwrap();
            tracker.decoded();
            picture
        };
        step(&mut tracker, 0, 0, &[]);
        step(&mut tracker, 1, 1, &[]);
        step(&mut tracker, 2, 2, &[(3, &[1, 0])]); // picture 2 turns frame_num 0 (2 back) into long-term 0
        assert_eq!(tracker.references().iter().map(|r| (r.key, r.long_term)).collect::<Vec<_>>(), vec![(0, Some(0)), (1, None), (2, None)]);
        step(&mut tracker, 3, 3, &[(1, &[1])]); // forget frame_num 1 (2 back from 3)
        assert_eq!(keys(&tracker), vec![0, 2, 3]);
        step(&mut tracker, 4, 4, &[(2, &[0]), (6, &[1])]); // forget long-term 0; current becomes long-term 1
        assert_eq!(tracker.references().iter().map(|r| (r.key, r.long_term)).collect::<Vec<_>>(), vec![(2, None), (3, None), (4, Some(1))]);
        step(&mut tracker, 5, 5, &[(4, &[1])]); // only long-term index 0 may remain: 1 goes
        assert_eq!(keys(&tracker), vec![2, 3, 5]);
        let picture = step(&mut tracker, 6, 6, &[(5, &[])]); // forget everything
        assert_eq!(picture.references.len(), 3);
        assert_eq!(tracker.references().iter().map(|r| (r.key, r.frame_num, r.top_field_order_cnt)).collect::<Vec<_>>(), vec![(6, 0, 0)]);
        // The numbering restarts after it: frame_num 1, lsb 2 is order 2.
        let nal = slice(false, true, SliceType::P, 1, 2, &[]);
        assert_eq!(tracker.begin(&nal, 7).unwrap().order(), 2);
    }

    #[test]
    fn splits_length_prefixed_samples() {
        let sample = [0, 0, 0, 2, 0x09, 0xf0, 0, 0, 0, 3, 0x65, 1, 2];
        let units: Vec<&[u8]> = nal_units(&sample, 4).map(Result::unwrap).collect();
        assert_eq!(units, vec![&[0x09, 0xf0][..], &[0x65, 1, 2][..]]);
        assert!(nal_units(&[0, 0, 0, 9, 1], 4).next().unwrap().is_err());
        assert!(nal_units(&[0, 0], 4).next().unwrap().is_err());
    }

    #[test]
    fn refuses_what_it_cannot_decode() {
        let mut tracker = tracker(4);
        assert!(tracker.begin(&[0x06, 0x05], 0).unwrap_err().contains("not a slice"));
        assert!(tracker.begin(&[0x65], 0).is_err());
        let fmo = Writer::default().ue(1).ue(0).u(1, 1).u(1, 0).ue(1).nal(0x68);
        assert!(tracker.parameter_set(&fmo).unwrap_err().contains("slice groups"));
        let orphan = Writer::default().ue(0).ue(7).ue(9).u(4, 0).nal(0x65);
        assert!(tracker.begin(&orphan, 0).unwrap_err().contains("unknown PPS"));
    }
}
