/**
 * Chromaticity coordinates of the source primaries.
 */

pub const AVCOL_PRI_RESERVED0: u32 = 0;
pub const AVCOL_PRI_BT709: u32 = 1; // also ITU-R BT1361 / IEC 61966-2-4 / SMPTE RP177 Annex B
pub const AVCOL_PRI_UNSPECIFIED: u32 = 2;
pub const AVCOL_PRI_RESERVED: u32 = 3;
pub const AVCOL_PRI_BT470M: u32 = 4; // also FCC Title 47 Code of Federal Regulations 73.682 (a)(20)
pub const AVCOL_PRI_BT470BG: u32 = 5; // also ITU-R BT601-6 625 / ITU-R BT1358 625 / ITU-R BT1700 625 PAL & SECAM
pub const AVCOL_PRI_SMPTE170M: u32 = 6; // also ITU-R BT601-6 525 / ITU-R BT1358 525 / ITU-R BT1700 NTSC
pub const AVCOL_PRI_SMPTE240M: u32 = 7; // functionally identical to above
pub const AVCOL_PRI_FILM: u32 = 8; // colour filters using Illuminant C
pub const AVCOL_PRI_BT2020: u32 = 9; // ITU-R BT2020
pub const AVCOL_PRI_SMPTE428: u32 = 10; // SMPTE ST 428-1 (CIE 1931 XYZ)
pub const AVCOL_PRI_SMPTEST428_1: u32 = 10; // Same as AVCOL_PRI_SMPTE428
pub const AVCOL_PRI_SMPTE431: u32 = 11; // SMPTE ST 431-2 (2011) / DCI P3
pub const AVCOL_PRI_SMPTE432: u32 = 12; // SMPTE ST 432-1 (2010) / P3 D65 / Display P3
pub const AVCOL_PRI_JEDEC_P22: u32 = 22; // JEDEC P22 phosphors
                                         // pub const AVCOL_PRI_NB: u32         = 23; // Not part of ABI

/**
 * Color Transfer Characteristic.
 */
pub const AVCOL_TRC_RESERVED0: u32 = 0;
pub const AVCOL_TRC_BT709: u32 = 1; // also ITU-R BT1361
pub const AVCOL_TRC_UNSPECIFIED: u32 = 2;
pub const AVCOL_TRC_RESERVED: u32 = 3;
pub const AVCOL_TRC_GAMMA22: u32 = 4; // also ITU-R BT470M / ITU-R BT1700 625 PAL & SECAM
pub const AVCOL_TRC_GAMMA28: u32 = 5; // also ITU-R BT470BG
pub const AVCOL_TRC_SMPTE170M: u32 = 6; // also ITU-R BT601-6 525 or 625 / ITU-R BT1358 525 or 625 / ITU-R BT1700 NTSC
pub const AVCOL_TRC_SMPTE240M: u32 = 7;
pub const AVCOL_TRC_LINEAR: u32 = 8; // "Linear transfer characteristics"
pub const AVCOL_TRC_LOG: u32 = 9; // "Logarithmic transfer characteristic (100:1 range)"
pub const AVCOL_TRC_LOG_SQRT: u32 = 10; // "Logarithmic transfer characteristic (100 * Sqrt(10) : 1 range)"
pub const AVCOL_TRC_IEC61966_2_4: u32 = 11; // IEC 61966-2-4
pub const AVCOL_TRC_BT1361_ECG: u32 = 12; // ITU-R BT1361 Extended Colour Gamut
pub const AVCOL_TRC_IEC61966_2_1: u32 = 13; // IEC 61966-2-1 (sRGB or sYCC)
pub const AVCOL_TRC_BT2020_10: u32 = 14; // ITU-R BT2020 for 10-bit system
pub const AVCOL_TRC_BT2020_12: u32 = 15; // ITU-R BT2020 for 12-bit system
pub const AVCOL_TRC_SMPTE2084: u32 = 16; // SMPTE ST 2084 for 10-, 12-, 14- and 16-bit systems
pub const AVCOL_TRC_SMPTEST2084: u32 = 16; // Same as AVCOL_TRC_SMPTE2084
pub const AVCOL_TRC_SMPTE428: u32 = 17; // SMPTE ST 428-1
pub const AVCOL_TRC_SMPTEST428_1: u32 = 17; // Same as AVCOL_TRC_SMPTE428
pub const AVCOL_TRC_ARIB_STD_B67: u32 = 18; // ARIB STD-B67, known as "Hybrid log-gamma"
pub const AVCOL_TRC_NB: u32 = 19; // Not part of ABI

/**
 * YUV colorspace type.
 */
pub const AVCOL_SPC_RGB: u32 = 0; // order of coefficients is actually GBR, also IEC 61966-2-1 (sRGB)
pub const AVCOL_SPC_BT709: u32 = 1; // also ITU-R BT1361 / IEC 61966-2-4 xvYCC709 / SMPTE RP177 Annex B
pub const AVCOL_SPC_UNSPECIFIED: u32 = 2;
pub const AVCOL_SPC_RESERVED: u32 = 3;
pub const AVCOL_SPC_FCC: u32 = 4; // FCC Title 47 Code of Federal Regulations 73.682 (a)(20)
pub const AVCOL_SPC_BT470BG: u32 = 5; // also ITU-R BT601-6 625 / ITU-R BT1358 625 / ITU-R BT1700 625 PAL & SECAM / IEC 61966-2-4 xvYCC601
pub const AVCOL_SPC_SMPTE170M: u32 = 6; // also ITU-R BT601-6 525 / ITU-R BT1358 525 / ITU-R BT1700 NTSC
pub const AVCOL_SPC_SMPTE240M: u32 = 7; // functionally identical to above
pub const AVCOL_SPC_YCGCO: u32 = 8; // Used by Dirac / VC-2 and H.264 FRext, see ITU-T SG16
pub const AVCOL_SPC_YCOCG: u32 = 8; // Same as AVCOL_SPC_YCGCO
pub const AVCOL_SPC_BT2020_NCL: u32 = 9; // ITU-R BT2020 non-constant luminance system
pub const AVCOL_SPC_BT2020_CL: u32 = 10; // ITU-R BT2020 constant luminance system
pub const AVCOL_SPC_SMPTE2085: u32 = 11; // SMPTE 2085, Y'D'zD'x
pub const AVCOL_SPC_NB: u32 = 12; // Not part of ABI

/**
 * MPEG vs JPEG YUV range.
 */
pub const AVCOL_RANGE_UNSPECIFIED: u32 = 0;
pub const AVCOL_RANGE_MPEG: u32 = 1; // the normal 219*2^(n-8) "MPEG" YUV ranges
pub const AVCOL_RANGE_JPEG: u32 = 2; // the normal     2^n-1   "JPEG" YUV ranges
pub const AVCOL_RANGE_NB: u32 = 3; // Not part of ABI

//  * Illustration showing the location of the first (top left) chroma sample of the
//  * image, the left shows only luma, the right
//  * shows the location of the chroma sample, the 2 could be imagined to overlay
//  * each other but are drawn separately due to limitations of ASCII
//  *
//  *                1st 2nd       1st 2nd horizontal luma sample positions
//  *                 v   v         v   v
//  *                 ______        ______
//  *1st luma line > |X   X ...    |3 4 X ...     X are luma samples,
//  *                |             |1 2           1-6 are possible chroma positions
//  *2nd luma line > |X   X ...    |5 6 X ...     0 is undefined/unknown position

pub const AVCHROMA_LOC_UNSPECIFIED: u32 = 0;
pub const AVCHROMA_LOC_LEFT: u32 = 1; // MPEG-2/4 4:2:0, H.264 default for 4:2:0
pub const AVCHROMA_LOC_CENTER: u32 = 2; // MPEG-1 4:2:0, JPEG 4:2:0, H.263 4:2:0
pub const AVCHROMA_LOC_TOPLEFT: u32 = 3; // ITU-R 601, SMPTE 274M 296M S314M(DV 4:1:1), mpeg2 4:2:2
pub const AVCHROMA_LOC_TOP: u32 = 4;
pub const AVCHROMA_LOC_BOTTOMLEFT: u32 = 5;
pub const AVCHROMA_LOC_BOTTOM: u32 = 6;
pub const AVCHROMA_LOC_NB: u32 = 7; // Not part of ABI
