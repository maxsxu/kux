//! Oklch lightness inversion matching K9s v0.51.0 and go-colorful v1.3.0.

use ratatui::style::Color;

/// Invert perceived lightness, retaining hue and at least half the chroma where possible.
/// Terminal default colors remain defaults; named/indexed colors use the xterm palette.
#[must_use]
pub fn invert_color(color: Color) -> Color {
    let Some(rgb) = rgb(color) else {
        return Color::Reset;
    };
    let [lightness, chroma, hue] = to_lch(rgb.map(|value| f64::from(value) / 255.0));
    let target = 1.0 - lightness;
    let channels = if chroma < 0.01 {
        from_lch(target, 0.0, hue)
    } else {
        let lightness = closest_lightness(target, chroma * 0.5, hue);
        from_lch(lightness, chroma.min(max_chroma(lightness, hue)), hue)
    };
    let [red, green, blue] = channels.map(to_byte);
    Color::Rgb(red, green, blue)
}

fn closest_lightness(target: f64, chroma: f64, hue: f64) -> f64 {
    if max_chroma(target, hue) >= chroma {
        return target;
    }
    let direction = if target < 0.5 { 0.01 } else { -0.01 };
    let mut lightness = target;
    while if direction > 0.0 {
        lightness <= 0.5
    } else {
        lightness >= 0.5
    } {
        if max_chroma(lightness, hue) >= chroma {
            return lightness;
        }
        lightness += direction;
    }
    lightness = if direction > 0.0 { 0.51 } else { 0.49 };
    while (0.05..=0.95).contains(&lightness) {
        if max_chroma(lightness, hue) >= chroma {
            return lightness;
        }
        lightness += direction;
    }
    target
}

fn max_chroma(lightness: f64, hue: f64) -> f64 {
    let (mut low, mut high) = (0.0, 0.4);
    while high - low > 0.001 {
        let middle = f64::midpoint(low, high);
        if from_lch(lightness, middle, hue)
            .iter()
            .all(|value| (0.0..=1.0).contains(value))
        {
            low = middle;
        } else {
            high = middle;
        }
    }
    low
}

fn linearize(value: f64) -> f64 {
    if value <= 0.04045 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

fn delinearize(value: f64) -> f64 {
    if value <= 0.003_130_8 {
        value * 12.92
    } else {
        1.055 * value.powf(1.0 / 2.4) - 0.055
    }
}

fn multiply(matrix: [[f64; 3]; 3], values: [f64; 3]) -> [f64; 3] {
    matrix.map(|row| row[0] * values[0] + row[1] * values[1] + row[2] * values[2])
}

// Retain the upstream conversion matrix precision so gamut search matches its fixtures.
#[allow(clippy::excessive_precision)]
fn to_lch(rgb: [f64; 3]) -> [f64; 3] {
    let xyz = multiply(
        [
            [
                0.412_390_799_265_959_48,
                0.357_584_339_383_877_96,
                0.180_480_788_401_834_29,
            ],
            [
                0.212_639_005_871_510_36,
                0.715_168_678_767_755_93,
                0.072_192_315_360_733_715,
            ],
            [
                0.019_330_818_715_591_851,
                0.119_194_779_794_625_99,
                0.950_532_152_249_660_58,
            ],
        ],
        rgb.map(linearize),
    );
    let lms = multiply(
        [
            [0.818_933_010_1, 0.361_866_742_4, -0.128_859_713_7],
            [0.032_984_543_6, 0.929_311_871_5, 0.036_145_638_7],
            [0.048_200_301_8, 0.264_366_269_1, 0.633_851_707_0],
        ],
        xyz,
    )
    .map(f64::cbrt);
    let lab = multiply(
        [
            [0.210_454_255_3, 0.793_617_785_0, -0.004_072_046_8],
            [1.977_998_495_1, -2.428_592_205_0, 0.450_593_709_9],
            [0.025_904_037_1, 0.782_771_766_2, -0.808_675_766_0],
        ],
        lms,
    );
    [
        lab[0],
        (lab[1] * lab[1] + lab[2] * lab[2]).sqrt(),
        lab[2].atan2(lab[1]),
    ]
}

#[allow(clippy::excessive_precision)]
fn from_lch(lightness: f64, chroma: f64, hue: f64) -> [f64; 3] {
    let lms = multiply(
        [
            [
                0.999_999_998_450_519_6,
                0.396_337_792_173_767_74,
                0.215_803_758_060_758_8,
            ],
            [
                1.000_000_008_881_760_7,
                -0.105_561_342_323_656_33,
                -0.063_854_174_771_705_9,
            ],
            [
                1.000_000_054_672_410_8,
                -0.089_484_182_094_965_74,
                -1.291_485_537_864_091_7,
            ],
        ],
        [lightness, chroma * hue.cos(), chroma * hue.sin()],
    )
    .map(|value| value.powi(3));
    let xyz = multiply(
        [
            [
                1.226_879_873_374_155_7,
                -0.557_814_996_555_481_3,
                0.281_391_050_177_215_94,
            ],
            [
                -0.040_575_762_624_313_72,
                1.112_286_829_397_059_4,
                -0.071_711_066_661_516_96,
            ],
            [
                -0.076_372_949_746_721_42,
                -0.421_493_323_962_791_6,
                1.586_924_024_427_242_2,
            ],
        ],
        lms,
    );
    multiply(
        [
            [
                3.240_969_941_904_521_4,
                -1.537_383_177_570_093_5,
                -0.498_610_760_293_003_28,
            ],
            [
                -0.969_243_636_280_879_83,
                1.875_967_501_507_720_7,
                0.041_555_057_407_175_613,
            ],
            [
                0.055_630_079_696_993_609,
                -0.203_976_958_888_976_57,
                1.056_971_514_242_878_6,
            ],
        ],
        xyz,
    )
    .map(delinearize)
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn to_byte(value: f64) -> u8 {
    (value.clamp(0.0, 1.0) * 255.0).round() as u8
}

fn rgb(color: Color) -> Option<[u8; 3]> {
    let index = match color {
        Color::Reset => return None,
        Color::Rgb(red, green, blue) => return Some([red, green, blue]),
        Color::Indexed(index) => index,
        Color::Black => 0,
        Color::Red => 1,
        Color::Green => 2,
        Color::Yellow => 3,
        Color::Blue => 4,
        Color::Magenta => 5,
        Color::Cyan => 6,
        Color::Gray => 7,
        Color::DarkGray => 8,
        Color::LightRed => 9,
        Color::LightGreen => 10,
        Color::LightYellow => 11,
        Color::LightBlue => 12,
        Color::LightMagenta => 13,
        Color::LightCyan => 14,
        Color::White => 15,
    };
    Some(match index {
        0..=15 => [
            [0, 0, 0],
            [128, 0, 0],
            [0, 128, 0],
            [128, 128, 0],
            [0, 0, 128],
            [128, 0, 128],
            [0, 128, 128],
            [192, 192, 192],
            [128, 128, 128],
            [255, 0, 0],
            [0, 255, 0],
            [255, 255, 0],
            [0, 0, 255],
            [255, 0, 255],
            [0, 255, 255],
            [255, 255, 255],
        ][usize::from(index)],
        16..=231 => {
            let cube = index - 16;
            [cube / 36, (cube / 6) % 6, cube % 6].map(
                |value| {
                    if value == 0 { 0 } else { 55 + value * 40 }
                },
            )
        }
        232..=255 => [8 + (index - 232) * 10; 3],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_k9s_v0_51_0_inversion_fixtures() {
        for (original, expected) in [
            (0x000_000, 0xfff_fff),
            (0xfff_fff, 0x000_000),
            (0xff0_000, 0x7e0_000),
            (0x000_0ff, 0x1f5_bff),
            (0x00f_f00, 0x006_600),
            (0xfff_f00, 0x656_501),
            (0x00f_fff, 0x016_464),
            (0x333_333, 0x989_898),
            (0xccc_ccc, 0x0c0_c0c),
            (0x636_363, 0x636_363),
        ] {
            assert_eq!(
                invert_color(Color::from_u32(original)),
                Color::from_u32(expected)
            );
        }
    }

    #[test]
    fn terminal_defaults_are_preserved_and_palette_colors_match_rgb() {
        assert_eq!(invert_color(Color::Reset), Color::Reset);
        for (indexed, direct) in [
            (Color::Black, Color::Rgb(0, 0, 0)),
            (Color::LightRed, Color::Rgb(255, 0, 0)),
            (Color::Indexed(21), Color::Rgb(0, 0, 255)),
            (Color::Indexed(232), Color::Rgb(8, 8, 8)),
            (Color::Indexed(255), Color::Rgb(238, 238, 238)),
        ] {
            assert_eq!(invert_color(indexed), invert_color(direct));
        }
    }

    #[test]
    fn upstream_achromatic_fixtures_round_trip() {
        for level in [0, 255, 128, 51, 204, 85, 99] {
            let original = Color::Rgb(level, level, level);
            assert_eq!(invert_color(invert_color(original)), original);
        }
    }
}
