//! Read-only, deliberately narrow TEXV0005/TEXI0001/TEXB0003 support.
//! No embedded image fallback, animation, cubemaps, encryption, or target execution.

use std::{fs::File, io::Read, path::Path};

use anyhow::{ensure, Context, Result};

const MAX_FILE_BYTES: usize = 64 * 1024 * 1024;
const MAX_RGBA_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct Texture {
    /// Stored mip-zero dimensions, including any right/bottom padding.
    pub width: u32,
    pub height: u32,
    /// Logical image dimensions. The three inspected Eagle Flag textures have no padding.
    pub image_width: u32,
    pub image_height: u32,
    pub format: u32,
    pub flags: u32,
    /// Row-major, straight RGBA8, with a stride of `width * 4` bytes.
    pub rgba: Vec<u8>,
}

/// Load native RGBA8 (format 0) or BC3/DXT5 (format 4), with raw or LZ4 blocks.
/// Both the encoded file and the sum of declared RGBA mip expansions are capped
/// at 64 MiB. Callers must additionally budget `rgba.len()` across retained textures.
pub fn load(path: &Path) -> Result<Texture> {
    let file = File::open(path).with_context(|| format!("cannot open TEX {}", path.display()))?;
    let metadata = file.metadata().context("cannot inspect TEX file")?;
    ensure!(metadata.is_file(), "TEX input is not a regular file");
    ensure!(
        metadata.len() <= MAX_FILE_BYTES as u64,
        "encoded TEX exceeds 64 MiB budget"
    );
    // Bound the read itself as well: the file may grow after the metadata check.
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .context("cannot read TEX file")?;
    decode(&bytes).with_context(|| format!("invalid TEX {}", path.display()))
}

impl Texture {
    /// Bilinear sampling over the logical image, not the padded extent.
    /// Texel centers are at (i + 0.5) / image_size; v increases in stored row order.
    /// Channels are interpolated as UNORM, without sRGB conversion, premultiplication,
    /// normal reconstruction, vertical flipping, or mip filtering. Nonfinite UVs
    /// return transparent black. Public fields must retain the invariants of `load`.
    /// Edge-clamped variant used for the base albedo quad. Normal/detail maps use repeat.
    pub fn sample_clamp(&self, u: f32, v: f32) -> [f32; 4] {
        if !u.is_finite() || !v.is_finite() {
            return [0.0; 4];
        }
        let x = f64::from(u.clamp(0.0, 1.0)) * f64::from(self.image_width) - 0.5;
        let y = f64::from(v.clamp(0.0, 1.0)) * f64::from(self.image_height) - 0.5;
        let x0 = x.floor() as i64;
        let y0 = y.floor() as i64;
        let tx = (x - x.floor()) as f32;
        let ty = (y - y.floor()) as f32;
        let clamp = |index: i64, size: u32| index.clamp(0, i64::from(size) - 1) as usize;
        let xs = [clamp(x0, self.image_width), clamp(x0 + 1, self.image_width)];
        let ys = [
            clamp(y0, self.image_height),
            clamp(y0 + 1, self.image_height),
        ];
        Self::interpolate(self, xs, ys, tx, ty)
    }

    /// Wrap-around variant used for animated normal/detail maps.
    pub fn sample_repeat(&self, u: f32, v: f32) -> [f32; 4] {
        if !u.is_finite() || !v.is_finite() {
            return [0.0; 4];
        }
        let x = f64::from(u).rem_euclid(1.0) * f64::from(self.image_width) - 0.5;
        let y = f64::from(v).rem_euclid(1.0) * f64::from(self.image_height) - 0.5;
        let x0 = x.floor() as i64;
        let y0 = y.floor() as i64;
        let tx = (x - x.floor()) as f32;
        let ty = (y - y.floor()) as f32;
        let xs = [x0, x0 + 1].map(|i| i.rem_euclid(i64::from(self.image_width)) as usize);
        let ys = [y0, y0 + 1].map(|i| i.rem_euclid(i64::from(self.image_height)) as usize);
        Self::interpolate(self, xs, ys, tx, ty)
    }

    fn interpolate(
        texture: &Texture,
        xs: [usize; 2],
        ys: [usize; 2],
        tx: f32,
        ty: f32,
    ) -> [f32; 4] {
        let offsets = [
            (ys[0] * texture.width as usize + xs[0]) * 4,
            (ys[0] * texture.width as usize + xs[1]) * 4,
            (ys[1] * texture.width as usize + xs[0]) * 4,
            (ys[1] * texture.width as usize + xs[1]) * 4,
        ];
        std::array::from_fn(|channel| {
            let [a, b, c, d] = offsets.map(|offset| f32::from(texture.rgba[offset + channel]));
            let top = a + (b - a) * tx;
            let bottom = c + (d - c) * tx;
            (top + (bottom - top) * ty) / 255.0
        })
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self
            .offset
            .checked_add(len)
            .context("TEX offset overflow")?;
        let bytes = self
            .bytes
            .get(self.offset..end)
            .with_context(|| format!("truncated TEX at byte {}, need {len} bytes", self.offset))?;
        self.offset = end;
        Ok(bytes)
    }

    fn word(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into()?))
    }

    fn tag(&mut self, expected: &[u8; 9]) -> Result<()> {
        let offset = self.offset;
        ensure!(
            self.take(9)? == expected,
            "unsupported TEX header/version at byte {offset}; expected {}",
            String::from_utf8_lossy(expected)
        );
        Ok(())
    }
}

fn rgba_len(width: u32, height: u32) -> Result<usize> {
    ensure!(width > 0 && height > 0, "TEX dimensions must be nonzero");
    let len = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|n| n.checked_mul(4))
        .context("TEX RGBA expansion overflow")?;
    ensure!(
        len <= MAX_RGBA_BYTES as u64,
        "TEX RGBA exceeds 64 MiB budget"
    );
    Ok(len as usize)
}

fn decode(bytes: &[u8]) -> Result<Texture> {
    ensure!(
        bytes.len() <= MAX_FILE_BYTES,
        "encoded TEX exceeds 64 MiB budget"
    );
    let mut reader = Reader { bytes, offset: 0 };
    reader.tag(b"TEXV0005\0")?;
    reader.tag(b"TEXI0001\0")?;
    let format = reader.word()?;
    ensure!(
        matches!(format, 0 | 4),
        "unsupported TEX format {format}; only RGBA8/BC3 are supported"
    );
    let flags = reader.word()?;
    ensure!(
        flags == 0 || flags == 0x2,
        "unsupported TEX flags {flags:#x}; only regular textures are supported"
    );
    let width = reader.word()?;
    let height = reader.word()?;
    let image_width = reader.word()?;
    let image_height = reader.word()?;
    let base_len = rgba_len(width, height)?;
    ensure!(
        image_width > 0 && image_height > 0 && image_width <= width && image_height <= height,
        "logical TEX dimensions must be nonzero and fit the stored dimensions"
    );
    // Opaque TEXI metadata, nonzero in every inspected sample; not a size or flag.
    let _metadata = reader.word()?;
    reader.tag(b"TEXB0003\0")?;
    let images = reader.word()?;
    ensure!(
        images == 1,
        "unsupported TEX image count {images}; expected one"
    );
    let image_format = reader.word()?;
    ensure!(
        image_format == u32::MAX,
        "unsupported TEX image format {image_format:#x}; expected native sentinel"
    );
    let mip_count = reader.word()?;
    let max_mips = u32::BITS - width.max(height).leading_zeros();
    ensure!(
        mip_count > 0 && mip_count <= max_mips,
        "invalid TEX mip count {mip_count}"
    );

    struct Mip<'a> {
        compression: u32,
        decoded_len: usize,
        payload: &'a [u8],
    }
    let mut mips = Vec::new();
    let mut total_rgba = 0usize;
    // Validate every record and its payload range before any image allocation.
    for level in 0..mip_count {
        let mw = reader.word()?;
        let mh = reader.word()?;
        ensure!(
            mw == (width >> level).max(1) && mh == (height >> level).max(1),
            "invalid TEX mip {level} dimensions {mw}x{mh}"
        );
        total_rgba = total_rgba
            .checked_add(rgba_len(mw, mh)?)
            .context("TEX mip expansion overflow")?;
        ensure!(
            total_rgba <= MAX_RGBA_BYTES,
            "TEX mip RGBA total exceeds 64 MiB budget"
        );
        let compression = reader.word()?;
        ensure!(
            matches!(compression, 0 | 1),
            "unsupported TEX mip {level} compression {compression}"
        );
        let decoded_len = reader.word()? as usize;
        let stored_len = reader.word()? as usize;
        let expected = if format == 0 {
            rgba_len(mw, mh)? as u64
        } else {
            // BC3 retains a whole 16-byte block even for 4x2 or 2x2 final mips.
            u64::from(mw.div_ceil(4)) * u64::from(mh.div_ceil(4)) * 16
        };
        ensure!(
            expected <= MAX_RGBA_BYTES as u64,
            "TEX mip {level} native expansion exceeds 64 MiB budget"
        );
        ensure!(
            decoded_len as u64 == expected,
            "TEX mip {level} decoded length {decoded_len} does not match expected {expected}"
        );
        ensure!(stored_len > 0, "empty TEX mip {level} payload");
        ensure!(
            compression != 0 || stored_len == decoded_len,
            "raw TEX mip {level} length mismatch"
        );
        let payload = reader
            .take(stored_len)
            .with_context(|| format!("TEX mip {level} payload"))?;
        mips.push(Mip {
            compression,
            decoded_len,
            payload,
        });
    }
    ensure!(
        reader.offset == bytes.len(),
        "unsupported trailing TEX data at byte {}",
        reader.offset
    );

    let mut rgba = vec![0; base_len];
    let mut scratch = Vec::new();
    for (level, mip) in mips.iter().enumerate() {
        let data = if mip.compression == 0 {
            mip.payload
        } else {
            // Exact bounded raw-block LZ4, not a frame or size-prefixed stream.
            scratch.resize(mip.decoded_len, 0);
            let written = lz4_flex::block::decompress_into(mip.payload, &mut scratch)
                .with_context(|| format!("invalid LZ4 TEX mip {level}"))?;
            ensure!(
                written == mip.decoded_len,
                "LZ4 TEX mip {level} decoded length {written} does not match expected {}",
                mip.decoded_len
            );
            &scratch
        };
        if level != 0 {
            continue;
        }
        if format == 0 {
            rgba.copy_from_slice(data);
        } else {
            let blocks_x = width.div_ceil(4) as usize;
            for (index, block) in data.as_chunks::<16>().0.iter().enumerate() {
                let bx = (index % blocks_x) * 4;
                let by = (index / blocks_x) * 4;
                let mut pixels = [0; 16];
                texture2ddecoder::decode_bc3_block(block, &mut pixels);
                for y in 0..4.min(height as usize - by) {
                    for x in 0..4.min(width as usize - bx) {
                        // texture2ddecoder packs BGRA in little-endian byte order.
                        let [b, g, r, a] = pixels[y * 4 + x].to_le_bytes();
                        let offset = ((by + y) * width as usize + bx + x) * 4;
                        rgba[offset..offset + 4].copy_from_slice(&[r, g, b, a]);
                    }
                }
            }
        }
    }
    Ok(Texture {
        width,
        height,
        image_width,
        image_height,
        format,
        flags,
        rgba,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        bytes: Vec<u8>,
        records: Vec<usize>,
    }

    impl Fixture {
        fn new(format: u32, width: u32, height: u32) -> Self {
            let mut bytes = Vec::new();
            bytes.extend_from_slice(b"TEXV0005\0TEXI0001\0");
            for word in [format, 0, width, height, width, height, 0xff123456] {
                bytes.extend_from_slice(&word.to_le_bytes());
            }
            bytes.extend_from_slice(b"TEXB0003\0");
            for word in [1u32, u32::MAX, 0] {
                bytes.extend_from_slice(&word.to_le_bytes());
            }
            Self {
                bytes,
                records: Vec::new(),
            }
        }

        fn word(&mut self, at: usize, value: u32) {
            self.bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
        }

        fn mip(&mut self, width: u32, height: u32, compression: u32, data: &[u8]) {
            self.records.push(self.bytes.len());
            let payload = if compression == 1 {
                lz4_flex::block::compress(data)
            } else {
                data.to_vec()
            };
            for word in [
                width,
                height,
                compression,
                data.len() as u32,
                payload.len() as u32,
            ] {
                self.bytes.extend_from_slice(&word.to_le_bytes());
            }
            self.bytes.extend_from_slice(&payload);
            self.word(63, self.records.len() as u32);
        }

        fn decode(&self) -> Result<Texture> {
            decode(&self.bytes)
        }

        fn rejects(&self, reason: &str) {
            let error = self.decode().unwrap_err();
            assert!(format!("{error:#}").contains(reason), "{error:#}");
        }
    }

    fn red_bc3(alpha: u8) -> [u8; 16] {
        let mut block = [0; 16];
        block[0] = alpha;
        block[8..10].copy_from_slice(&0xf800u16.to_le_bytes());
        block
    }

    #[test]
    fn native_rgba_and_lz4_preserve_channels_and_metadata() {
        for compression in [0, 1] {
            let pixels = [10, 20, 30, 40, 50, 60, 70, 80];
            let mut fixture = Fixture::new(0, 2, 1);
            fixture.mip(2, 1, compression, &pixels);
            let texture = fixture.decode().unwrap();
            assert_eq!((texture.width, texture.height), (2, 1));
            assert_eq!((texture.image_width, texture.image_height), (2, 1));
            assert_eq!((texture.format, texture.flags), (0, 0));
            assert_eq!(texture.rgba, pixels);
        }
    }

    #[test]
    fn bc3_raw_and_lz4_handle_partial_blocks_and_small_mips() {
        for compression in [0, 1] {
            let mut fixture = Fixture::new(4, 5, 3);
            let blocks = [red_bc3(37), red_bc3(201)].concat();
            fixture.mip(5, 3, compression, &blocks);
            fixture.mip(2, 1, 0, &red_bc3(255));
            let texture = fixture.decode().unwrap();
            assert_eq!(texture.rgba.len(), 5 * 3 * 4);
            for y in 0..3 {
                for x in 0..5 {
                    let offset = (y * 5 + x) * 4;
                    assert_eq!(
                        &texture.rgba[offset..offset + 4],
                        &[255, 0, 0, if x < 4 { 37 } else { 201 }]
                    );
                }
            }
        }
        let mut fixture = Fixture::new(4, 2, 2);
        fixture.mip(2, 2, 0, &red_bc3(255));
        assert_eq!(fixture.decode().unwrap().rgba, [255, 0, 0, 255].repeat(4));
    }

    #[test]
    fn bc3_block_rows_preserve_orientation_and_rgba_order() {
        let mut blue = red_bc3(255);
        blue[8..10].copy_from_slice(&0x001fu16.to_le_bytes());
        let mut fixture = Fixture::new(4, 4, 5);
        fixture.mip(4, 5, 0, &[red_bc3(255), blue].concat());
        let texture = fixture.decode().unwrap();
        assert_eq!(&texture.rgba[..4 * 4 * 4], &[255, 0, 0, 255].repeat(16));
        assert_eq!(&texture.rgba[4 * 4 * 4..], &[0, 0, 255, 255].repeat(4));
    }

    #[test]
    fn rejects_every_truncated_prefix_including_later_mips() {
        let mut fixture = Fixture::new(4, 8, 4);
        fixture.mip(8, 4, 1, &red_bc3(255).repeat(2));
        fixture.mip(4, 2, 0, &red_bc3(255));
        fixture.decode().unwrap();
        for length in 0..fixture.bytes.len() {
            assert!(
                decode(&fixture.bytes[..length]).is_err(),
                "accepted prefix of length {length}"
            );
        }
    }

    #[test]
    fn rejects_unknown_versions_and_malformed_tags() {
        for at in [0, 7, 8, 9, 16, 17, 46, 53, 54] {
            let mut fixture = Fixture::new(0, 1, 1);
            fixture.mip(1, 1, 0, &[0; 4]);
            fixture.bytes[at] ^= 1;
            fixture.rejects("unsupported TEX header/version");
        }
    }

    #[test]
    fn rejects_unsupported_formats_flags_images_and_compression() {
        for (at, value, reason) in [
            (18, 1, "unsupported TEX format"),
            (18, 7, "unsupported TEX format"),
            (18, u32::MAX, "unsupported TEX format"),
            (22, 1, "unsupported TEX flags"),
            (55, 0, "unsupported TEX image count"),
            (55, 2, "unsupported TEX image count"),
            (59, 0, "unsupported TEX image format"),
            (75, 2, "unsupported TEX mip 0 compression"),
        ] {
            let mut fixture = Fixture::new(0, 1, 1);
            fixture.mip(1, 1, 0, &[0; 4]);
            fixture.word(at, value);
            fixture.rejects(reason);
        }
    }

    #[test]
    fn rejects_invalid_dimensions_and_expansion_before_payload_allocation() {
        for (width, height, reason) in [
            (0, 1, "dimensions must be nonzero"),
            (1, 0, "dimensions must be nonzero"),
            (4097, 4096, "64 MiB budget"),
            (u32::MAX, u32::MAX, "expansion overflow"),
        ] {
            Fixture::new(4, width, height).rejects(reason);
        }
        for (at, value) in [(34, 0), (38, 0), (34, 5), (38, 5)] {
            let mut fixture = Fixture::new(4, 4, 4);
            fixture.word(at, value);
            fixture.rejects("logical TEX dimensions");
        }
        let mut fixture = Fixture::new(4, 4096, 4096);
        fixture.mip(4096, 4096, 0, &[0; 16]);
        fixture.word(63, 2);
        fixture.word(79, 4096 * 4096);
        fixture.word(83, 4096 * 4096);
        // Supply a correctly sized BC3 base so the second mip's RGBA sum is checked.
        fixture.bytes.resize(87 + 4096 * 4096, 0);
        fixture.mip(2048, 2048, 0, &[]);
        fixture.rejects("mip RGBA total exceeds 64 MiB");
    }

    #[test]
    fn rejects_encoded_file_budget_in_memory() {
        assert!(decode(&vec![0; MAX_FILE_BYTES + 1])
            .unwrap_err()
            .to_string()
            .contains("encoded TEX exceeds"));
    }

    #[test]
    fn rejects_mip_counts_dimensions_lengths_and_trailing_bytes() {
        for (at, value, reason) in [
            (63, 0, "invalid TEX mip count"),
            (63, u32::MAX, "invalid TEX mip count"),
            (67, 2, "mip 0 dimensions"),
            (71, 2, "mip 0 dimensions"),
            (79, 15, "decoded length"),
            (83, 0, "empty TEX mip"),
            (83, 15, "length mismatch"),
            (83, u32::MAX, "length mismatch"),
        ] {
            let mut fixture = Fixture::new(4, 4, 4);
            fixture.mip(4, 4, 0, &red_bc3(255));
            fixture.word(at, value);
            fixture.rejects(reason);
        }
        let mut fixture = Fixture::new(0, 1, 1);
        fixture.mip(1, 1, 0, &[0; 4]);
        fixture.bytes.push(0);
        fixture.rejects("unsupported trailing TEX data");
    }

    #[test]
    fn validates_later_mip_metadata_and_payload_boundaries() {
        for (field, value, reason) in [
            (0, 3, "mip 1 dimensions"),
            (8, 2, "mip 1 compression"),
            (12, 15, "mip 1 decoded length"),
            (16, u32::MAX, "TEX mip 1 payload"),
        ] {
            let mut fixture = Fixture::new(4, 8, 4);
            fixture.mip(8, 4, 0, &red_bc3(255).repeat(2));
            fixture.mip(4, 2, 1, &red_bc3(255));
            fixture.word(fixture.records[1] + field, value);
            fixture.rejects(reason);
        }
    }

    #[test]
    fn rejects_lz4_short_output_overflow_and_corruption_in_any_mip() {
        for level in [0, 1] {
            for data in [vec![0; 15], vec![0; 17], vec![0xff]] {
                let mut fixture = Fixture::new(4, 4, 4);
                if level == 1 {
                    fixture.mip(4, 4, 0, &red_bc3(255));
                }
                let size = if level == 0 { 4 } else { 2 };
                fixture.mip(size, size, 1, &data);
                fixture.word(fixture.records[level] + 12, 16);
                fixture.rejects(if data.len() < 16 {
                    "decoded length"
                } else {
                    "invalid LZ4"
                });
            }
            let mut fixture = Fixture::new(4, 4, 4);
            if level == 1 {
                fixture.mip(4, 4, 0, &red_bc3(255));
            }
            let size = if level == 0 { 4 } else { 2 };
            fixture.mip(size, size, 1, &red_bc3(255));
            let at = fixture.records[level];
            fixture.bytes.truncate(at + 20);
            fixture.bytes.push(0xff); // Literal-length extension is missing.
            fixture.word(at + 16, 1);
            fixture.rejects("invalid LZ4");
        }
    }

    #[test]
    fn sampler_uses_logical_repeat_centers_seams_and_row_stride() {
        let mut fixture = Fixture::new(0, 3, 3);
        fixture.word(34, 2);
        fixture.word(38, 2);
        fixture.mip(
            3,
            3,
            0,
            &[
                255, 0, 0, 255, 0, 255, 0, 255, 99, 99, 99, 99, 0, 0, 255, 255, 255, 255, 255, 255,
                99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
            ],
        );
        let texture = fixture.decode().unwrap();
        assert_eq!((texture.width, texture.height), (3, 3));
        assert_eq!((texture.image_width, texture.image_height), (2, 2));
        for (u, v, expected) in [
            (0.25, 0.25, [1.0, 0.0, 0.0, 1.0]),
            (0.75, 0.25, [0.0, 1.0, 0.0, 1.0]),
            (0.25, 0.75, [0.0, 0.0, 1.0, 1.0]),
            (-0.25, 1.25, [0.0, 1.0, 0.0, 1.0]),
            (0.5, 0.25, [0.5, 0.5, 0.0, 1.0]),
            (0.25, 0.0, [0.5, 0.0, 0.5, 1.0]),
            (0.0, 0.0, [0.5, 0.5, 0.5, 1.0]),
            (1.0, -1.0, [0.5, 0.5, 0.5, 1.0]),
            (f32::MAX, f32::MIN, [0.5, 0.5, 0.5, 1.0]),
        ] {
            assert_eq!(texture.sample_repeat(u, v), expected, "uv {u}, {v}");
        }
        for uv in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert_eq!(texture.sample_repeat(uv, 0.0), [0.0; 4]);
            assert_eq!(texture.sample_repeat(0.0, uv), [0.0; 4]);
        }
    }

    #[test]
    #[ignore = "requires WALLPAPER_TEST_SCENE_ROOT pointing to a local Eagle Flag project"]
    fn loads_shipped_eagle_flag_textures_when_explicitly_selected() {
        let Some(root) = std::env::var_os("WALLPAPER_TEST_SCENE_ROOT") else {
            eprintln!("skipping shipped texture check without WALLPAPER_TEST_SCENE_ROOT");
            return;
        };
        for (name, width, height) in [
            ("eagle", 2048, 1024),
            ("flag_normal", 32, 32),
            ("cloth", 512, 512),
        ] {
            let texture = load(
                &std::path::Path::new(&root)
                    .join("materials")
                    .join(format!("{name}.tex")),
            )
            .unwrap();
            assert_eq!(
                (
                    texture.format,
                    texture.flags,
                    texture.width,
                    texture.height,
                    texture.image_width,
                    texture.image_height,
                    texture.rgba.len()
                ),
                (
                    4,
                    0,
                    width,
                    height,
                    width,
                    height,
                    width as usize * height as usize * 4
                )
            );
            assert!(texture.rgba.iter().any(|&byte| byte != 0));
            let png = image::open(
                std::path::Path::new(&root)
                    .join("materials")
                    .join(format!("{name}.png")),
            )
            .unwrap()
            .into_rgba8();
            // Eagle and cloth are ordinary color/detail bitmaps; flag_normal is a
            // DXT5n-packed normal map whose stored channels intentionally differ.
            if name != "flag_normal" {
                let mean = texture
                    .rgba
                    .iter()
                    .zip(png.as_raw().iter())
                    .map(|(&a, &b)| a.abs_diff(b) as u64)
                    .sum::<u64>() as f64
                    / texture.rgba.len() as f64;
                assert!(mean < 5.0, "{name} native-vs-png mean abs diff: {mean:.3}");
            }
        }
    }

    #[test]
    fn sampler_single_texel_preserves_straight_alpha_and_unorm() {
        let mut fixture = Fixture::new(0, 1, 1);
        fixture.mip(1, 1, 0, &[51, 102, 153, 0]);
        let texture = fixture.decode().unwrap();
        assert_eq!(texture.sample_repeat(-2.5, 3.7), [0.2, 0.4, 0.6, 0.0]);
    }
}
