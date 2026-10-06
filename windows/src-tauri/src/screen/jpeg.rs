// A baseline JPEG encoder: RGB pixels in, a JFIF file out, in memory.
//
// Why this exists. A screenshot has to be small to send (a full PNG of a screen is
// megabytes) and the app takes no third-party dependencies it can avoid. Windows
// could do it through COM, but that is far more code than this, ties the encoder to
// one platform, and cannot be tested without a desktop. This is ~200 lines of a very
// old, very settled format: 4:4:4 (no chroma subsampling, so coloured text stays
// sharp), the standard quantisation tables scaled by `quality`, and the standard
// Huffman tables.
//
// It is tested against a small decoder written for the tests (see below), so "the
// file is a valid JPEG that looks like the input" is checked in CI, not assumed.

const ZIGZAG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

// The example quantisation tables of the JPEG standard (Annex K), in natural order.
const LUMA_Q: [u16; 64] = [
    16, 11, 10, 16, 24, 40, 51, 61, 12, 12, 14, 19, 26, 58, 60, 55, 14, 13, 16, 24, 40, 57, 69, 56,
    14, 17, 22, 29, 51, 87, 80, 62, 18, 22, 37, 56, 68, 109, 103, 77, 24, 35, 55, 64, 81, 104, 113,
    92, 49, 64, 78, 87, 103, 121, 120, 101, 72, 92, 95, 98, 112, 100, 103, 99,
];
const CHROMA_Q: [u16; 64] = [
    17, 18, 24, 47, 99, 99, 99, 99, 18, 21, 26, 66, 99, 99, 99, 99, 24, 26, 56, 99, 99, 99, 99, 99,
    47, 66, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
    99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
];

// The standard Huffman tables (Annex K.3): how many codes of each length 1..=16, then
// the symbols in code order. Any complete table that holds every symbol the encoder
// emits would decode correctly, since the table travels in the file; these are the
// ones tuned for ordinary pictures, which keeps files small.
const DC_LUMA_BITS: [u8; 16] = [0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0];
const DC_CHROMA_BITS: [u8; 16] = [0, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0];
const DC_VALUES: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];

const AC_LUMA_BITS: [u8; 16] = [0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 0x7d];
const AC_LUMA_VALUES: [u8; 162] = [
    0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06, 0x13, 0x51, 0x61, 0x07,
    0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xa1, 0x08, 0x23, 0x42, 0xb1, 0xc1, 0x15, 0x52, 0xd1, 0xf0,
    0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0a, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x25, 0x26, 0x27, 0x28,
    0x29, 0x2a, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49,
    0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69,
    0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89,
    0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7,
    0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5,
    0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe1, 0xe2,
    0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8,
    0xf9, 0xfa,
];
const AC_CHROMA_BITS: [u8; 16] = [0, 2, 1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1, 2, 0x77];
const AC_CHROMA_VALUES: [u8; 162] = [
    0x00, 0x01, 0x02, 0x03, 0x11, 0x04, 0x05, 0x21, 0x31, 0x06, 0x12, 0x41, 0x51, 0x07, 0x61, 0x71,
    0x13, 0x22, 0x32, 0x81, 0x08, 0x14, 0x42, 0x91, 0xa1, 0xb1, 0xc1, 0x09, 0x23, 0x33, 0x52, 0xf0,
    0x15, 0x62, 0x72, 0xd1, 0x0a, 0x16, 0x24, 0x34, 0xe1, 0x25, 0xf1, 0x17, 0x18, 0x19, 0x1a, 0x26,
    0x27, 0x28, 0x29, 0x2a, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48,
    0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68,
    0x69, 0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87,
    0x88, 0x89, 0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5,
    0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3,
    0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda,
    0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8,
    0xf9, 0xfa,
];

#[derive(Debug, PartialEq, Eq)]
pub enum EncodeError {
    /// Width or height is zero, or more than a JPEG can describe (65,535).
    Size,
    /// The pixel buffer is not width × height × 3 bytes.
    Buffer,
}

/// The longest side a JPEG can have.
pub const MAX_SIDE: u32 = 65_535;

/// Encodes `rgb` (three bytes per pixel, row after row) as a baseline JPEG.
/// `quality` is 1 (smallest) to 100 (best); out-of-range values are clamped.
pub fn encode(width: u32, height: u32, rgb: &[u8], quality: u8) -> Result<Vec<u8>, EncodeError> {
    if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE {
        return Err(EncodeError::Size);
    }
    if rgb.len() != width as usize * height as usize * 3 {
        return Err(EncodeError::Buffer);
    }
    let luma_q = scaled(&LUMA_Q, quality);
    let chroma_q = scaled(&CHROMA_Q, quality);
    let dc_luma = Codes::new(&DC_LUMA_BITS, &DC_VALUES);
    let dc_chroma = Codes::new(&DC_CHROMA_BITS, &DC_VALUES);
    let ac_luma = Codes::new(&AC_LUMA_BITS, &AC_LUMA_VALUES);
    let ac_chroma = Codes::new(&AC_CHROMA_BITS, &AC_CHROMA_VALUES);

    let mut out = Vec::with_capacity(width as usize * height as usize / 4 + 1024);
    out.extend_from_slice(&[0xFF, 0xD8]); // SOI
                                          // JFIF header: version 1.01, no density, no thumbnail.
    out.extend_from_slice(&[
        0xFF, 0xE0, 0, 16, b'J', b'F', b'I', b'F', 0, 1, 1, 0, 0, 1, 0, 1, 0, 0,
    ]);
    for (id, table) in [(0u8, &luma_q), (1u8, &chroma_q)] {
        out.extend_from_slice(&[0xFF, 0xDB, 0, 67, id]);
        out.extend(ZIGZAG.iter().map(|&i| table[i] as u8));
    }
    out.extend_from_slice(&[0xFF, 0xC0, 0, 17, 8]); // SOF0: baseline, 8-bit
    out.extend_from_slice(&(height as u16).to_be_bytes());
    out.extend_from_slice(&(width as u16).to_be_bytes());
    out.extend_from_slice(&[3, 1, 0x11, 0, 2, 0x11, 1, 3, 0x11, 1]);
    for (class_and_id, bits, values) in [
        (0x00u8, &DC_LUMA_BITS[..], &DC_VALUES[..]),
        (0x10, &AC_LUMA_BITS[..], &AC_LUMA_VALUES[..]),
        (0x01, &DC_CHROMA_BITS[..], &DC_VALUES[..]),
        (0x11, &AC_CHROMA_BITS[..], &AC_CHROMA_VALUES[..]),
    ] {
        out.extend_from_slice(&[0xFF, 0xC4]);
        out.extend_from_slice(&((19 + values.len()) as u16).to_be_bytes());
        out.push(class_and_id);
        out.extend_from_slice(bits);
        out.extend_from_slice(values);
    }
    out.extend_from_slice(&[0xFF, 0xDA, 0, 12, 3, 1, 0x00, 2, 0x11, 3, 0x11, 0, 63, 0]); // SOS

    let mut bits = BitWriter::new(&mut out);
    let mut previous_dc = [0i32; 3];
    let cosines = cosine_table();
    let (w, h) = (width as usize, height as usize);
    for block_y in (0..h).step_by(8) {
        for block_x in (0..w).step_by(8) {
            let mut planes = [[0f32; 64]; 3];
            for y in 0..8 {
                for x in 0..8 {
                    // Pixels past the edge repeat the last one, so the blocks there are smooth.
                    let at = ((block_y + y).min(h - 1) * w + (block_x + x).min(w - 1)) * 3;
                    let (r, g, b) = (rgb[at] as f32, rgb[at + 1] as f32, rgb[at + 2] as f32);
                    planes[0][y * 8 + x] = 0.299 * r + 0.587 * g + 0.114 * b - 128.0;
                    planes[1][y * 8 + x] = -0.168_736 * r - 0.331_264 * g + 0.5 * b;
                    planes[2][y * 8 + x] = 0.5 * r - 0.418_688 * g - 0.081_312 * b;
                }
            }
            for (component, plane) in planes.iter().enumerate() {
                let (quant, dc, ac) = if component == 0 {
                    (&luma_q, &dc_luma, &ac_luma)
                } else {
                    (&chroma_q, &dc_chroma, &ac_chroma)
                };
                let coefficients = quantise(&dct(plane, &cosines), quant);
                write_block(
                    &mut bits,
                    &coefficients,
                    &mut previous_dc[component],
                    dc,
                    ac,
                );
            }
        }
    }
    bits.finish();
    out.extend_from_slice(&[0xFF, 0xD9]); // EOI
    Ok(out)
}

/// A quantisation table for `quality`, in natural order.
fn scaled(base: &[u16; 64], quality: u8) -> [u16; 64] {
    let quality = u32::from(quality.clamp(1, 100));
    let scale = if quality < 50 {
        5000 / quality
    } else {
        200 - 2 * quality
    };
    base.map(|b| ((u32::from(b) * scale + 50) / 100).clamp(1, 255) as u16)
}

/// The Huffman code of every symbol: (code, length).
struct Codes([(u16, u8); 256]);

impl Codes {
    fn new(counts: &[u8; 16], symbols: &[u8]) -> Codes {
        let mut table = [(0u16, 0u8); 256];
        let (mut code, mut next) = (0u16, 0usize);
        for (length, &count) in (1u8..=16).zip(counts) {
            for _ in 0..count {
                table[symbols[next] as usize] = (code, length);
                code += 1;
                next += 1;
            }
            code <<= 1;
        }
        Codes(table)
    }
}

struct BitWriter<'a> {
    out: &'a mut Vec<u8>,
    buffer: u32,
    filled: u32,
}

impl<'a> BitWriter<'a> {
    fn new(out: &'a mut Vec<u8>) -> Self {
        BitWriter {
            out,
            buffer: 0,
            filled: 0,
        }
    }

    fn put(&mut self, value: u32, length: u32) {
        if length == 0 {
            return;
        }
        self.buffer = (self.buffer << length) | (value & ((1 << length) - 1));
        self.filled += length;
        while self.filled >= 8 {
            let byte = (self.buffer >> (self.filled - 8)) as u8;
            self.out.push(byte);
            if byte == 0xFF {
                self.out.push(0); // a data byte of 0xFF is escaped, so it is not a marker
            }
            self.filled -= 8;
        }
        self.buffer &= (1 << self.filled) - 1;
    }

    /// Pads the last byte with ones, as the standard asks.
    fn finish(&mut self) {
        if self.filled > 0 {
            let pad = 8 - self.filled;
            self.put((1 << pad) - 1, pad);
        }
    }
}

/// cos((2x + 1)·u·π / 16), scaled for the 1-D transform (u = 0 gets 1/√2, all halved).
fn cosine_table() -> [[f32; 8]; 8] {
    let mut table = [[0f32; 8]; 8];
    for (u, row) in table.iter_mut().enumerate() {
        let c = if u == 0 {
            std::f32::consts::FRAC_1_SQRT_2
        } else {
            1.0
        };
        for (x, cell) in row.iter_mut().enumerate() {
            *cell = 0.5 * c * (((2 * x + 1) as f32 * u as f32 * std::f32::consts::PI) / 16.0).cos();
        }
    }
    table
}

/// The 8×8 forward DCT, rows then columns.
fn dct(block: &[f32; 64], cosines: &[[f32; 8]; 8]) -> [f32; 64] {
    let mut rows = [0f32; 64];
    for y in 0..8 {
        for u in 0..8 {
            rows[y * 8 + u] = (0..8).map(|x| block[y * 8 + x] * cosines[u][x]).sum();
        }
    }
    let mut out = [0f32; 64];
    for u in 0..8 {
        for v in 0..8 {
            out[v * 8 + u] = (0..8).map(|y| rows[y * 8 + u] * cosines[v][y]).sum();
        }
    }
    out
}

fn quantise(coefficients: &[f32; 64], table: &[u16; 64]) -> [i32; 64] {
    let mut out = [0i32; 64];
    for ((o, c), q) in out.iter_mut().zip(coefficients).zip(table) {
        *o = (c / f32::from(*q)).round() as i32;
    }
    out
}

/// How many bits `value` needs (its JPEG "size" category): 0 for 0, 1 for ±1, 2 for ±2..3.
fn category(value: i32) -> u32 {
    32 - value.unsigned_abs().leading_zeros()
}

/// The bits that carry `value` in `size` bits: itself, or for a negative number its
/// one's complement.
fn amplitude(value: i32, size: u32) -> u32 {
    if value >= 0 {
        value as u32
    } else {
        (value - 1) as u32 & ((1 << size) - 1)
    }
}

fn write_block(
    bits: &mut BitWriter,
    coefficients: &[i32; 64],
    previous_dc: &mut i32,
    dc: &Codes,
    ac: &Codes,
) {
    let difference = coefficients[0] - *previous_dc;
    *previous_dc = coefficients[0];
    let size = category(difference);
    let (code, length) = dc.0[size as usize];
    bits.put(u32::from(code), u32::from(length));
    bits.put(amplitude(difference, size), size);

    let mut run = 0;
    for &i in &ZIGZAG[1..] {
        let value = coefficients[i];
        if value == 0 {
            run += 1;
            continue;
        }
        while run > 15 {
            let (code, length) = ac.0[0xF0]; // sixteen zeros
            bits.put(u32::from(code), u32::from(length));
            run -= 16;
        }
        let size = category(value);
        let (code, length) = ac.0[(run << 4 | size) as usize];
        bits.put(u32::from(code), u32::from(length));
        bits.put(amplitude(value, size), size);
        run = 0;
    }
    if run > 0 {
        let (code, length) = ac.0[0x00]; // end of block
        bits.put(u32::from(code), u32::from(length));
    }
}

#[cfg(test)]
#[allow(clippy::needless_range_loop)]
mod tests {
    use super::*;

    // ── A decoder, for the tests only ────────────────────────────────────────
    //
    // Just enough of one to read back what `encode` writes: baseline, three
    // components of 1×1, the tables in the file. If this reproduces the picture, the
    // file is valid, and its Huffman tables and quantisation are the ones it says.

    struct Decoded {
        width: usize,
        height: usize,
        rgb: Vec<u8>,
    }

    struct HuffmanTable {
        min_code: [i32; 17],
        max_code: [i32; 18],
        first_symbol: [usize; 17],
        symbols: Vec<u8>,
    }

    impl HuffmanTable {
        fn new(counts: &[u8], symbols: &[u8]) -> HuffmanTable {
            let mut t = HuffmanTable {
                min_code: [0; 17],
                max_code: [-1; 18],
                first_symbol: [0; 17],
                symbols: symbols.to_vec(),
            };
            let (mut code, mut index) = (0i32, 0usize);
            for length in 1..=16 {
                t.first_symbol[length] = index;
                t.min_code[length] = code;
                code += i32::from(counts[length - 1]);
                index += counts[length - 1] as usize;
                t.max_code[length] = if counts[length - 1] > 0 { code - 1 } else { -1 };
                code <<= 1;
            }
            t
        }
    }

    struct Reader<'a> {
        data: &'a [u8],
        at: usize,
        bit: u32,
    }

    impl Reader<'_> {
        fn bit(&mut self) -> u32 {
            let byte = self.data[self.at];
            let value = u32::from(byte >> (7 - self.bit)) & 1;
            self.bit += 1;
            if self.bit == 8 {
                self.bit = 0;
                self.at += 1;
                if byte == 0xFF {
                    assert_eq!(self.data[self.at], 0, "a 0xFF in the data must be escaped");
                    self.at += 1;
                }
            }
            value
        }
        fn bits(&mut self, count: u32) -> i32 {
            (0..count).fold(0i32, |acc, _| (acc << 1) | self.bit() as i32)
        }
        fn symbol(&mut self, table: &HuffmanTable) -> u8 {
            let mut code = 0i32;
            for length in 1..=16 {
                code = (code << 1) | self.bit() as i32;
                if table.max_code[length] >= 0 && code <= table.max_code[length] {
                    return table.symbols
                        [table.first_symbol[length] + (code - table.min_code[length]) as usize];
                }
            }
            panic!("no such code");
        }
        /// A signed value of `size` bits as `amplitude` wrote it.
        fn amplitude(&mut self, size: u32) -> i32 {
            if size == 0 {
                return 0;
            }
            let v = self.bits(size);
            if v < (1 << (size - 1)) {
                v - (1 << size) + 1
            } else {
                v
            }
        }
    }

    fn decode(file: &[u8]) -> Decoded {
        assert_eq!(&file[..2], &[0xFF, 0xD8], "starts with SOI");
        assert_eq!(&file[file.len() - 2..], &[0xFF, 0xD9], "ends with EOI");
        let mut quant: [[u16; 64]; 2] = [[0; 64]; 2];
        let mut tables: [Option<HuffmanTable>; 4] = [None, None, None, None];
        let (mut width, mut height) = (0usize, 0usize);
        let mut at = 2;
        let scan_start = loop {
            assert_eq!(file[at], 0xFF, "a marker at {at}");
            let marker = file[at + 1];
            let length = u16::from_be_bytes([file[at + 2], file[at + 3]]) as usize;
            let body = &file[at + 4..at + 2 + length];
            match marker {
                0xDB => {
                    let id = (body[0] & 15) as usize;
                    for (k, &z) in ZIGZAG.iter().enumerate() {
                        quant[id][z] = u16::from(body[1 + k]);
                    }
                }
                0xC0 => {
                    height = u16::from_be_bytes([body[1], body[2]]) as usize;
                    width = u16::from_be_bytes([body[3], body[4]]) as usize;
                    assert_eq!(body[5], 3, "three components");
                }
                0xC4 => {
                    let slot = ((body[0] >> 4) * 2 + (body[0] & 15)) as usize; // AC/DC × id
                    let total: usize = body[1..17].iter().map(|&n| n as usize).sum();
                    assert_eq!(body.len(), 17 + total, "the table says what it holds");
                    tables[slot] = Some(HuffmanTable::new(&body[1..17], &body[17..]));
                }
                0xDA => break at + 2 + length,
                _ => {}
            }
            at += 2 + length;
        };
        let mut reader = Reader {
            data: &file[..file.len() - 2],
            at: scan_start,
            bit: 0,
        };
        let mut rgb = vec![0u8; width * height * 3];
        let mut dc = [0i32; 3];
        let cos = cosine_table();
        for block_y in (0..height).step_by(8) {
            for block_x in (0..width).step_by(8) {
                let mut planes = [[0f32; 64]; 3];
                for c in 0..3 {
                    let (dc_t, ac_t) = if c == 0 {
                        (&tables[0], &tables[2])
                    } else {
                        (&tables[1], &tables[3])
                    };
                    let (dc_t, ac_t) = (dc_t.as_ref().unwrap(), ac_t.as_ref().unwrap());
                    let q = &quant[usize::from(c > 0)];
                    let size = reader.symbol(dc_t) as u32;
                    dc[c] += reader.amplitude(size);
                    let mut coefficients = [0f32; 64];
                    coefficients[0] = (dc[c] * i32::from(q[0])) as f32;
                    let mut k = 1;
                    while k < 64 {
                        let symbol = reader.symbol(ac_t);
                        if symbol == 0x00 {
                            break;
                        }
                        k += (symbol >> 4) as usize;
                        if symbol == 0xF0 {
                            k += 1;
                            continue;
                        }
                        let value = reader.amplitude(u32::from(symbol & 15));
                        let natural = ZIGZAG[k];
                        coefficients[natural] = (value * i32::from(q[natural])) as f32;
                        k += 1;
                    }
                    // Inverse DCT: the transpose of the forward one.
                    let mut rows = [0f32; 64];
                    for v in 0..8 {
                        for x in 0..8 {
                            rows[v * 8 + x] =
                                (0..8).map(|u| coefficients[v * 8 + u] * cos[u][x]).sum();
                        }
                    }
                    for y in 0..8 {
                        for x in 0..8 {
                            planes[c][y * 8 + x] =
                                (0..8).map(|v| rows[v * 8 + x] * cos[v][y]).sum();
                        }
                    }
                }
                for y in 0..8 {
                    for x in 0..8 {
                        let (px, py) = (block_x + x, block_y + y);
                        if px >= width || py >= height {
                            continue;
                        }
                        let (luma, cb, cr) = (
                            planes[0][y * 8 + x] + 128.0,
                            planes[1][y * 8 + x],
                            planes[2][y * 8 + x],
                        );
                        let at = (py * width + px) * 3;
                        rgb[at] = (luma + 1.402 * cr).round().clamp(0.0, 255.0) as u8;
                        rgb[at + 1] = (luma - 0.344_136 * cb - 0.714_136 * cr)
                            .round()
                            .clamp(0.0, 255.0) as u8;
                        rgb[at + 2] = (luma + 1.772 * cb).round().clamp(0.0, 255.0) as u8;
                    }
                }
            }
        }
        Decoded { width, height, rgb }
    }

    /// Mean absolute difference per channel value between two pictures.
    fn mean_error(a: &[u8], b: &[u8]) -> f64 {
        assert_eq!(a.len(), b.len());
        a.iter()
            .zip(b)
            .map(|(&x, &y)| f64::from(x.abs_diff(y)))
            .sum::<f64>()
            / a.len() as f64
    }

    /// A picture with smooth colour, hard edges and fine detail, like a screen.
    fn sample(width: usize, height: usize) -> Vec<u8> {
        let mut rgb = Vec::with_capacity(width * height * 3);
        for y in 0..height {
            for x in 0..width {
                let (r, g, b) =
                    if (y / 4) % 2 == 0 && x % 11 < 3 && y > height / 3 && y < height / 2 {
                        (20, 20, 20) // thin dark strokes, like text
                    } else if y < height / 3 {
                        ((x * 255 / width) as u8, (y * 255 / height) as u8, 128)
                    // a gradient
                    } else if x < width / 3 {
                        (230, 40, 40) // a flat red panel
                    } else {
                        (245, 245, 245) // a white page
                    };
                rgb.extend_from_slice(&[r, g, b]);
            }
        }
        rgb
    }

    #[test]
    fn the_standard_tables_are_complete_and_hold_every_symbol_the_encoder_emits() {
        for (bits, values) in [
            (&AC_LUMA_BITS[..], &AC_LUMA_VALUES[..]),
            (&AC_CHROMA_BITS[..], &AC_CHROMA_VALUES[..]),
        ] {
            assert_eq!(
                bits.iter().map(|&n| n as usize).sum::<usize>(),
                values.len()
            );
            // Every (run, size) the encoder can write, plus end-of-block and sixteen-zeros.
            let mut wanted: Vec<u8> = vec![0x00, 0xF0];
            wanted.extend((0..16u8).flat_map(|run| (1..=10u8).map(move |size| run << 4 | size)));
            let mut have = values.to_vec();
            have.sort_unstable();
            wanted.sort_unstable();
            assert_eq!(have, wanted, "exactly the 162 symbols, once each");
            // A prefix code that is not over-full, and never uses the all-ones code.
            let kraft: f64 = bits
                .iter()
                .enumerate()
                .map(|(i, &n)| f64::from(n) / 2f64.powi(i as i32 + 1))
                .sum();
            assert!(kraft < 1.0, "{kraft}");
        }
        for bits in [&DC_LUMA_BITS, &DC_CHROMA_BITS] {
            assert_eq!(
                bits.iter().map(|&n| n as usize).sum::<usize>(),
                DC_VALUES.len()
            );
        }
    }

    #[test]
    fn a_picture_comes_back_as_itself() {
        for (w, h) in [(64, 48), (200, 120), (17, 9), (8, 8), (1, 1), (3, 70)] {
            let original = sample(w, h);
            let file = encode(w as u32, h as u32, &original, 85).unwrap();
            let back = decode(&file);
            assert_eq!((back.width, back.height), (w, h));
            let error = mean_error(&original, &back.rgb);
            // One or two blocks hold the whole picture's detail, so they are looser.
            let allowed = if w * h <= 300 { 10.0 } else { 6.0 };
            assert!(error < allowed, "{w}x{h}: mean error {error}");
        }
    }

    #[test]
    fn a_flat_colour_comes_back_exactly_enough() {
        for colour in [[0u8, 0, 0], [255, 255, 255], [10, 200, 90], [128, 128, 128]] {
            let original: Vec<u8> = (0..32 * 32).flat_map(|_| colour).collect();
            let back = decode(&encode(32, 32, &original, 90).unwrap());
            assert!(mean_error(&original, &back.rgb) < 2.0, "{colour:?}");
        }
    }

    #[test]
    fn higher_quality_is_bigger_and_closer() {
        let original = sample(160, 100);
        let small = encode(160, 100, &original, 30).unwrap();
        let big = encode(160, 100, &original, 90).unwrap();
        assert!(big.len() > small.len());
        let (e_small, e_big) = (
            mean_error(&original, &decode(&small).rgb),
            mean_error(&original, &decode(&big).rgb),
        );
        assert!(e_big < e_small, "{e_big} vs {e_small}");
        // And a screenshot-like picture is a small fraction of its raw size.
        assert!(
            big.len() < original.len() / 4,
            "{} of {}",
            big.len(),
            original.len()
        );
    }

    #[test]
    fn the_file_is_well_formed() {
        let file = encode(33, 17, &sample(33, 17), 80).unwrap();
        assert_eq!(&file[..4], &[0xFF, 0xD8, 0xFF, 0xE0], "SOI then JFIF");
        assert_eq!(&file[6..11], b"JFIF\0");
        // Height and width in the frame header, big-endian.
        let sof = file.windows(2).position(|w| w == [0xFF, 0xC0]).unwrap();
        assert_eq!(u16::from_be_bytes([file[sof + 5], file[sof + 6]]), 17);
        assert_eq!(u16::from_be_bytes([file[sof + 7], file[sof + 8]]), 33);
        assert_eq!(file[sof + 9], 3, "three components");
        // Four quantisation/Huffman tables are in the file, in the order the scan uses.
        assert_eq!(file.windows(2).filter(|w| *w == [0xFF, 0xDB]).count(), 2);
        assert_eq!(file.windows(2).filter(|w| *w == [0xFF, 0xC4]).count(), 4);
    }

    #[test]
    fn bytes_that_look_like_markers_in_the_data_are_escaped() {
        // Noise makes 0xFF data bytes likely; the decoder asserts each one is followed by 0x00.
        let mut seed = 7u32;
        let noise: Vec<u8> = (0..64 * 64 * 3)
            .map(|_| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (seed >> 24) as u8
            })
            .collect();
        let file = encode(64, 64, &noise, 95).unwrap();
        let scan = file.windows(2).position(|w| w == [0xFF, 0xDA]).unwrap() + 14;
        let data = &file[scan..file.len() - 2];
        for (i, &b) in data.iter().enumerate() {
            if b == 0xFF {
                assert_eq!(data[i + 1], 0x00, "an unescaped 0xFF at {i}");
            }
        }
        assert_eq!(decode(&file).width, 64);
    }

    #[test]
    fn bad_input_is_refused_not_encoded() {
        assert_eq!(encode(0, 10, &[], 80), Err(EncodeError::Size));
        assert_eq!(encode(10, 0, &[], 80), Err(EncodeError::Size));
        assert_eq!(encode(65_536, 1, &[], 80), Err(EncodeError::Size));
        assert_eq!(encode(2, 2, &[0; 11], 80), Err(EncodeError::Buffer));
        assert_eq!(encode(2, 2, &[0; 13], 80), Err(EncodeError::Buffer));
        // A quality outside 1..=100 is clamped, not an error.
        assert!(encode(8, 8, &[100; 192], 0).is_ok());
        assert!(encode(8, 8, &[100; 192], 250).is_ok());
    }

    #[test]
    fn the_same_picture_encodes_to_the_same_bytes() {
        let original = sample(40, 30);
        assert_eq!(encode(40, 30, &original, 80), encode(40, 30, &original, 80));
    }
}
