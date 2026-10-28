//! Small QR encoder for IPv4 terminal sharing URLs (version 6, ECC M).
//! Fixed-size symbols hold up to 106 UTF-8 bytes; four RS blocks protect each
//! symbol. No network request, external QR service, or image file is involved.
use super::*;
const SIDE: usize = 41;
const DATA: usize = 108;
const ECC: usize = 16;

pub(super) struct Code {
    modules: Vec<bool>,
    reserved: Vec<bool>,
}
impl Code {
    pub fn encode(text: &str) -> Result<Self> {
        let bytes = text.as_bytes();
        if bytes.len() > 106 {
            bail!("This sharing link is too long for a QR code. Copy the link instead.");
        }
        let mut bits = vec![];
        let mut append = |value: usize, count: usize| {
            bits.extend((0..count).rev().map(|bit| value >> bit & 1 != 0));
        };
        append(4, 4); // Byte mode, with an eight-bit length for versions 1–9.
        append(bytes.len(), 8);
        for byte in bytes {
            append(usize::from(*byte), 8);
        }
        bits.extend([false; 4]);
        while bits.len() % 8 != 0 {
            bits.push(false);
        }
        let mut data: Vec<u8> = bits
            .chunks(8)
            .map(|chunk| {
                chunk
                    .iter()
                    .fold(0, |value, bit| value << 1 | u8::from(*bit))
            })
            .collect();
        let mut pad = 0xec;
        while data.len() < DATA {
            data.push(pad);
            pad ^= 0xec ^ 0x11;
        }
        let blocks: Vec<_> = data
            .chunks(27)
            .map(|block| (block, remainder(block)))
            .collect();
        let mut codewords = vec![];
        for i in 0..27 {
            for (block, _) in &blocks {
                codewords.push(block[i]);
            }
        }
        for i in 0..ECC {
            for (_, ecc) in &blocks {
                codewords.push(ecc[i]);
            }
        }
        let mut code = Self {
            modules: vec![false; SIDE * SIDE],
            reserved: vec![false; SIDE * SIDE],
        };
        code.patterns();
        let mut index = 0;
        let mut right = SIDE as i32 - 1;
        while right >= 1 {
            if right == 6 {
                right = 5;
            }
            for vertical in 0..SIDE {
                let y = if (right + 1) & 2 == 0 {
                    SIDE - 1 - vertical
                } else {
                    vertical
                };
                for offset in 0..2 {
                    let x = right as usize - offset;
                    if !code.reserved[y * SIDE + x] {
                        code.modules[y * SIDE + x] = index < codewords.len() * 8
                            && codewords[index / 8] >> (7 - index % 8) & 1 != 0;
                        index += 1;
                    }
                }
            }
            right -= 2;
        }
        debug_assert_eq!(index, 1383); // 172 codewords and seven remainder bits.
        let original = code.modules.clone();
        let mut best = (usize::MAX, original.clone());
        for mask in 0..8 {
            code.modules.clone_from(&original);
            for y in 0..SIDE {
                for x in 0..SIDE {
                    let apply = match mask {
                        0 => (x + y) % 2 == 0,
                        1 => y % 2 == 0,
                        2 => x % 3 == 0,
                        3 => (x + y) % 3 == 0,
                        4 => (x / 3 + y / 2) % 2 == 0,
                        5 => x * y % 2 + x * y % 3 == 0,
                        6 => (x * y % 2 + x * y % 3) % 2 == 0,
                        _ => ((x + y) % 2 + x * y % 3) % 2 == 0,
                    };
                    if apply && !code.reserved[y * SIDE + x] {
                        code.modules[y * SIDE + x] ^= true;
                    }
                }
            }
            code.format(mask);
            let penalty = code.penalty();
            if penalty < best.0 {
                best = (penalty, code.modules.clone());
            }
        }
        code.modules = best.1;
        Ok(code)
    }
    fn function(&mut self, x: usize, y: usize, dark: bool) {
        self.modules[y * SIDE + x] = dark;
        self.reserved[y * SIDE + x] = true;
    }
    fn patterns(&mut self) {
        for i in 0..SIDE {
            self.function(6, i, i % 2 == 0);
            self.function(i, 6, i % 2 == 0);
        }
        for (cx, cy) in [(3, 3), (SIDE as i32 - 4, 3), (3, SIDE as i32 - 4)] {
            for dy in -4i32..=4 {
                for dx in -4i32..=4 {
                    let (x, y) = (cx + dx, cy + dy);
                    let distance = dx.abs().max(dy.abs());
                    if x >= 0 && y >= 0 && x < SIDE as i32 && y < SIDE as i32 {
                        self.function(x as usize, y as usize, distance != 2 && distance != 4);
                    }
                }
            }
        }
        for dy in -2i32..=2 {
            for dx in -2i32..=2 {
                self.function(
                    (34 + dx) as usize,
                    (34 + dy) as usize,
                    dx.abs().max(dy.abs()) != 1,
                );
            }
        }
        self.format(0);
    }
    fn format(&mut self, mask: usize) {
        // ECC M has format value zero. BCH(15,5), then the standard XOR mask.
        let mut remainder = mask;
        for _ in 0..10 {
            remainder = (remainder << 1) ^ ((remainder >> 9) * 0x537);
        }
        let bits = (mask << 10 | remainder) ^ 0x5412;
        let bit = |i| bits >> i & 1 != 0;
        for i in 0..6 {
            self.function(8, i, bit(i));
        }
        self.function(8, 7, bit(6));
        self.function(8, 8, bit(7));
        self.function(7, 8, bit(8));
        for i in 9..15 {
            self.function(14 - i, 8, bit(i));
        }
        for i in 0..8 {
            self.function(SIDE - 1 - i, 8, bit(i));
        }
        for i in 8..15 {
            self.function(8, SIDE - 15 + i, bit(i));
        }
        self.function(8, SIDE - 8, true);
    }
    fn penalty(&self) -> usize {
        let mut score = 0;
        for vertical in [false, true] {
            for line in 0..SIDE {
                let row: Vec<_> = (0..SIDE)
                    .map(|i| {
                        self.modules[if vertical {
                            i * SIDE + line
                        } else {
                            line * SIDE + i
                        }]
                    })
                    .collect();
                let mut run = 1;
                for i in 1..SIDE {
                    if row[i] == row[i - 1] {
                        run += 1;
                        if run == 5 {
                            score += 3;
                        } else if run > 5 {
                            score += 1;
                        }
                    } else {
                        run = 1;
                    }
                }
                for window in row.windows(11) {
                    let value = window
                        .iter()
                        .fold(0usize, |value, bit| value << 1 | usize::from(*bit));
                    if value == 0b00001011101 || value == 0b10111010000 {
                        score += 40;
                    }
                }
            }
        }
        for y in 0..SIDE - 1 {
            for x in 0..SIDE - 1 {
                let color = self.modules[y * SIDE + x];
                if [y * SIDE + x + 1, (y + 1) * SIDE + x, (y + 1) * SIDE + x + 1]
                    .iter()
                    .all(|i| self.modules[*i] == color)
                {
                    score += 3;
                }
            }
        }
        let dark = self.modules.iter().filter(|v| **v).count();
        score + (dark * 20).abs_diff(SIDE * SIDE * 10) / (SIDE * SIDE) * 10
    }
    pub fn show(&self, ui: &mut egui::Ui) {
        let module = ((ui.available_width() * ui.ctx().pixels_per_point() / (SIDE + 8) as f32)
            .floor()
            .min(8.0)
            / ui.ctx().pixels_per_point())
        .max(1.0);
        let size = module * (SIDE + 8) as f32;
        let (rect, _) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
        let painter = ui.painter();
        painter.rect_filled(rect, 0.0, Color32::WHITE);
        for y in 0..SIDE {
            for x in 0..SIDE {
                if self.modules[y * SIDE + x] {
                    let min =
                        rect.min + egui::vec2((x + 4) as f32 * module, (y + 4) as f32 * module);
                    painter.rect_filled(
                        egui::Rect::from_min_size(min, egui::vec2(module, module)),
                        0.0,
                        Color32::BLACK,
                    );
                }
            }
        }
    }
}
fn multiply(mut x: u8, mut y: u8) -> u8 {
    let mut result = 0;
    while y != 0 {
        if y & 1 != 0 {
            result ^= x;
        }
        x = x << 1 ^ if x & 0x80 != 0 { 0x1d } else { 0 };
        y >>= 1;
    }
    result
}
fn remainder(data: &[u8]) -> [u8; ECC] {
    let mut generator = [0; ECC];
    generator[ECC - 1] = 1;
    let mut root = 1;
    for _ in 0..ECC {
        for i in 0..ECC {
            generator[i] = multiply(generator[i], root);
            if i + 1 < ECC {
                generator[i] ^= generator[i + 1];
            }
        }
        root = multiply(root, 2);
    }
    let mut result = [0; ECC];
    for byte in data {
        let factor = byte ^ result[0];
        result.rotate_left(1);
        result[ECC - 1] = 0;
        for i in 0..ECC {
            result[i] ^= multiply(generator[i], factor);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn qr_matches_an_independent_reference_encoder() -> Result<()> {
        // Generated with the independent qrcode-terminal encoder: version 6,
        // ECC M, mask 2. This is synthetic login data, not a real credential.
        let code = Code::encode(
            "http://glimcode:0123456789abcdef0123456789abcdef0123@192.168.1.20:7681/",
        )?;
        let expected: Vec<_> = include_str!("qr_reference.txt")
            .bytes()
            .filter(|byte| matches!(byte, b'0' | b'1'))
            .map(|byte| byte == b'1')
            .collect();
        assert_eq!(code.modules, expected);
        Ok(())
    }

    #[test]
    fn qr_payload_boundaries_are_checked_in_bytes() {
        assert!(Code::encode(&"x".repeat(106)).is_ok());
        assert!(Code::encode(&"x".repeat(107)).is_err());
        assert!(Code::encode(&"猫".repeat(36)).is_err());
    }
    #[test]
    #[ignore = "exports a synthetic QR for independent decoder verification; set GLIMCODE_QR_TEST_PNG"]
    fn export_qr_for_decoder() -> Result<()> {
        let payload = std::env::var("GLIMCODE_QR_TEST_PAYLOAD").unwrap_or_else(|_| {
            "http://glimcode:0123456789abcdef0123456789abcdef0123@192.168.1.20:7681/".into()
        });
        let code = Code::encode(&payload)?;
        if let Ok(path) = std::env::var("GLIMCODE_QR_TEST_MATRIX") {
            let matrix: String = code
                .modules
                .chunks(SIDE)
                .map(|row| {
                    row.iter()
                        .map(|dark| if *dark { '1' } else { '0' })
                        .collect::<String>()
                        + "\n"
                })
                .collect();
            std::fs::write(path, matrix)?;
        }
        let scale = 8;
        let side = (SIDE + 8) * scale;
        let mut pixels = vec![255u8; side * side];
        for y in 0..SIDE {
            for x in 0..SIDE {
                if code.modules[y * SIDE + x] {
                    for dy in 0..scale {
                        for dx in 0..scale {
                            pixels[((y + 4) * scale + dy) * side + (x + 4) * scale + dx] = 0;
                        }
                    }
                }
            }
        }
        image::save_buffer(
            std::env::var("GLIMCODE_QR_TEST_PNG")?,
            &pixels,
            side as u32,
            side as u32,
            image::ColorType::L8,
        )?;
        Ok(())
    }
}
