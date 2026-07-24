use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RgbaColor {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
    pub alpha: u8,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HsvColor {
    pub hue: f32,
    pub saturation: f32,
    pub value: f32,
    pub alpha: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HslColor {
    pub hue: f32,
    pub saturation: f32,
    pub lightness: f32,
    pub alpha: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CmykColor {
    pub cyan: f32,
    pub magenta: f32,
    pub yellow: f32,
    pub key: f32,
    pub alpha: f32,
}

impl RgbaColor {
    pub const TRANSPARENT: Self = Self::new(0, 0, 0, 0);
    pub const BLACK: Self = Self::new(0, 0, 0, 255);
    pub const WHITE: Self = Self::new(255, 255, 255, 255);
    pub const RED: Self = Self::new(232, 68, 68, 255);

    pub const fn new(red: u8, green: u8, blue: u8, alpha: u8) -> Self {
        Self {
            red,
            green,
            blue,
            alpha,
        }
    }

    pub fn hex_rgb(self) -> String {
        format!("#{:02X}{:02X}{:02X}", self.red, self.green, self.blue)
    }

    pub fn hex_rgba(self) -> String {
        format!(
            "#{:02X}{:02X}{:02X}{:02X}",
            self.red, self.green, self.blue, self.alpha
        )
    }

    pub fn display_hex(self) -> String {
        if self.alpha == 255 {
            self.hex_rgb()
        } else {
            self.hex_rgba()
        }
    }

    pub fn parse_hex(value: &str) -> Result<Self, String> {
        let value = value.trim().trim_start_matches('#');
        let expanded = match value.len() {
            3 | 4 => value
                .chars()
                .flat_map(|character| [character, character])
                .collect::<String>(),
            6 | 8 => value.to_string(),
            _ => {
                return Err("颜色必须为 #RGB、#RGBA、#RRGGBB 或 #RRGGBBAA。".to_string());
            }
        };
        let channel = |offset: usize| {
            u8::from_str_radix(&expanded[offset..offset + 2], 16)
                .map_err(|_| "颜色包含无效的十六进制字符。".to_string())
        };
        Ok(Self::new(
            channel(0)?,
            channel(2)?,
            channel(4)?,
            if expanded.len() == 8 {
                channel(6)?
            } else {
                255
            },
        ))
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        let value = value.trim();
        if value.starts_with('#') || matches!(value.len(), 3 | 4 | 6 | 8) {
            return Self::parse_hex(value);
        }
        let lower = value.to_ascii_lowercase();
        if lower.starts_with("rgb") {
            let channels = color_arguments(value)?;
            if !(3..=4).contains(&channels.len()) {
                return Err("RGB 颜色需要 3 个通道和可选透明度。".to_string());
            }
            return Ok(Self::new(
                parse_u8_channel(channels[0])?,
                parse_u8_channel(channels[1])?,
                parse_u8_channel(channels[2])?,
                channels
                    .get(3)
                    .map(|value| parse_alpha(value))
                    .transpose()?
                    .unwrap_or(255),
            ));
        }
        if lower.starts_with("hsv") {
            let channels = color_arguments(value)?;
            if !(3..=4).contains(&channels.len()) {
                return Err("HSV 颜色需要 H、S、V 和可选透明度。".to_string());
            }
            return Ok(Self::from_hsv(HsvColor {
                hue: parse_number(channels[0])?,
                saturation: parse_percentage(channels[1])?,
                value: parse_percentage(channels[2])?,
                alpha: channels
                    .get(3)
                    .map(|value| parse_alpha(value).map(|alpha| alpha as f32 / 255.0))
                    .transpose()?
                    .unwrap_or(1.0),
            }));
        }
        Err("颜色支持 HEX、rgb(...) 或 hsv(...) 格式。".to_string())
    }

    pub fn with_alpha_factor(self, factor: f32) -> Self {
        Self {
            alpha: (self.alpha as f32 * factor.clamp(0.0, 1.0)).round() as u8,
            ..self
        }
    }

    pub fn relative_luminance(self) -> f32 {
        let linear = |channel: u8| {
            let value = channel as f32 / 255.0;
            if value <= 0.04045 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        linear(self.red) * 0.2126 + linear(self.green) * 0.7152 + linear(self.blue) * 0.0722
    }

    pub fn to_hsv(self) -> HsvColor {
        let red = self.red as f32 / 255.0;
        let green = self.green as f32 / 255.0;
        let blue = self.blue as f32 / 255.0;
        let maximum = red.max(green).max(blue);
        let minimum = red.min(green).min(blue);
        let delta = maximum - minimum;
        let hue = if delta <= f32::EPSILON {
            0.0
        } else if maximum == red {
            60.0 * ((green - blue) / delta).rem_euclid(6.0)
        } else if maximum == green {
            60.0 * ((blue - red) / delta + 2.0)
        } else {
            60.0 * ((red - green) / delta + 4.0)
        };
        HsvColor {
            hue,
            saturation: if maximum <= f32::EPSILON {
                0.0
            } else {
                delta / maximum
            },
            value: maximum,
            alpha: self.alpha as f32 / 255.0,
        }
    }

    pub fn from_hsv(value: HsvColor) -> Self {
        let hue = value.hue.rem_euclid(360.0) / 60.0;
        let saturation = value.saturation.clamp(0.0, 1.0);
        let brightness = value.value.clamp(0.0, 1.0);
        let chroma = brightness * saturation;
        let secondary = chroma * (1.0 - (hue.rem_euclid(2.0) - 1.0).abs());
        let (red, green, blue) = match hue.floor() as i32 {
            0 => (chroma, secondary, 0.0),
            1 => (secondary, chroma, 0.0),
            2 => (0.0, chroma, secondary),
            3 => (0.0, secondary, chroma),
            4 => (secondary, 0.0, chroma),
            _ => (chroma, 0.0, secondary),
        };
        let offset = brightness - chroma;
        Self::new(
            ((red + offset) * 255.0).round() as u8,
            ((green + offset) * 255.0).round() as u8,
            ((blue + offset) * 255.0).round() as u8,
            (value.alpha.clamp(0.0, 1.0) * 255.0).round() as u8,
        )
    }

    pub fn to_hsl(self) -> HslColor {
        let red = self.red as f32 / 255.0;
        let green = self.green as f32 / 255.0;
        let blue = self.blue as f32 / 255.0;
        let maximum = red.max(green).max(blue);
        let minimum = red.min(green).min(blue);
        let delta = maximum - minimum;
        let lightness = (maximum + minimum) / 2.0;
        let saturation = if delta <= f32::EPSILON {
            0.0
        } else {
            delta / (1.0 - (2.0 * lightness - 1.0).abs())
        };
        HslColor {
            hue: self.to_hsv().hue,
            saturation,
            lightness,
            alpha: self.alpha as f32 / 255.0,
        }
    }

    pub fn from_hsl(value: HslColor) -> Self {
        let hue = value.hue.rem_euclid(360.0) / 60.0;
        let saturation = value.saturation.clamp(0.0, 1.0);
        let lightness = value.lightness.clamp(0.0, 1.0);
        let chroma = (1.0 - (2.0 * lightness - 1.0).abs()) * saturation;
        let secondary = chroma * (1.0 - (hue.rem_euclid(2.0) - 1.0).abs());
        let (red, green, blue) = match hue.floor() as i32 {
            0 => (chroma, secondary, 0.0),
            1 => (secondary, chroma, 0.0),
            2 => (0.0, chroma, secondary),
            3 => (0.0, secondary, chroma),
            4 => (secondary, 0.0, chroma),
            _ => (chroma, 0.0, secondary),
        };
        let offset = lightness - chroma / 2.0;
        Self::new(
            ((red + offset) * 255.0).round() as u8,
            ((green + offset) * 255.0).round() as u8,
            ((blue + offset) * 255.0).round() as u8,
            (value.alpha.clamp(0.0, 1.0) * 255.0).round() as u8,
        )
    }

    pub fn to_cmyk(self) -> CmykColor {
        let red = self.red as f32 / 255.0;
        let green = self.green as f32 / 255.0;
        let blue = self.blue as f32 / 255.0;
        let key = 1.0 - red.max(green).max(blue);
        let denominator = 1.0 - key;
        let (cyan, magenta, yellow) = if denominator <= f32::EPSILON {
            (0.0, 0.0, 0.0)
        } else {
            (
                (1.0 - red - key) / denominator,
                (1.0 - green - key) / denominator,
                (1.0 - blue - key) / denominator,
            )
        };
        CmykColor {
            cyan,
            magenta,
            yellow,
            key,
            alpha: self.alpha as f32 / 255.0,
        }
    }

    pub fn format_rgb(self) -> String {
        format!(
            "rgb({}, {}, {}, {:.0}%)",
            self.red,
            self.green,
            self.blue,
            self.alpha as f32 / 255.0 * 100.0
        )
    }

    pub fn format_hsv(self) -> String {
        let hsv = self.to_hsv();
        format!(
            "hsv({:.0}°, {:.0}%, {:.0}%, {:.0}%)",
            hsv.hue,
            hsv.saturation * 100.0,
            hsv.value * 100.0,
            hsv.alpha * 100.0
        )
    }

    pub fn format_hsl(self) -> String {
        let hsl = self.to_hsl();
        format!(
            "hsl({:.0}°, {:.0}%, {:.0}%, {:.0}%)",
            hsl.hue,
            hsl.saturation * 100.0,
            hsl.lightness * 100.0,
            hsl.alpha * 100.0
        )
    }

    pub fn format_cmyk(self) -> String {
        let cmyk = self.to_cmyk();
        format!(
            "cmyk({:.0}%, {:.0}%, {:.0}%, {:.0}%, {:.0}%)",
            cmyk.cyan * 100.0,
            cmyk.magenta * 100.0,
            cmyk.yellow * 100.0,
            cmyk.key * 100.0,
            cmyk.alpha * 100.0
        )
    }
}

fn color_arguments(value: &str) -> Result<Vec<&str>, String> {
    let start = value
        .find('(')
        .ok_or_else(|| "颜色缺少左括号。".to_string())?;
    let end = value
        .rfind(')')
        .ok_or_else(|| "颜色缺少右括号。".to_string())?;
    if end <= start {
        return Err("颜色参数格式无效。".to_string());
    }
    Ok(value[start + 1..end]
        .split([',', ' '])
        .map(str::trim)
        .filter(|part| !part.is_empty() && *part != "/")
        .collect())
}

fn parse_number(value: &str) -> Result<f32, String> {
    value
        .trim()
        .trim_end_matches('°')
        .parse::<f32>()
        .map_err(|_| format!("无效的颜色数值：{value}"))
}

fn parse_u8_channel(value: &str) -> Result<u8, String> {
    if value.trim().ends_with('%') {
        return Ok((parse_percentage(value)? * 255.0).round() as u8);
    }
    Ok(parse_number(value)?.clamp(0.0, 255.0).round() as u8)
}

fn parse_percentage(value: &str) -> Result<f32, String> {
    let trimmed = value.trim();
    let number = parse_number(trimmed.trim_end_matches('%'))?;
    Ok(if trimmed.ends_with('%') || number > 1.0 {
        (number / 100.0).clamp(0.0, 1.0)
    } else {
        number.clamp(0.0, 1.0)
    })
}

fn parse_alpha(value: &str) -> Result<u8, String> {
    let trimmed = value.trim();
    let number = parse_number(trimmed.trim_end_matches('%'))?;
    let normalized = if trimmed.ends_with('%') {
        number / 100.0
    } else if number > 1.0 {
        number / 255.0
    } else {
        number
    };
    Ok((normalized.clamp(0.0, 1.0) * 255.0).round() as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_supported_hex_forms() {
        assert_eq!(
            RgbaColor::parse_hex("#F04").unwrap(),
            RgbaColor::new(255, 0, 68, 255)
        );
        assert_eq!(
            RgbaColor::parse_hex("#F048").unwrap(),
            RgbaColor::new(255, 0, 68, 136)
        );
        assert_eq!(
            RgbaColor::parse_hex("#12A0FF").unwrap(),
            RgbaColor::new(18, 160, 255, 255)
        );
        assert_eq!(
            RgbaColor::parse_hex("#12A0FF80").unwrap(),
            RgbaColor::new(18, 160, 255, 128)
        );
    }

    #[test]
    fn parses_rgb_hsv_and_alpha_forms() {
        assert_eq!(
            RgbaColor::parse("rgb(255, 0, 68, 50%)").unwrap(),
            RgbaColor::new(255, 0, 68, 128)
        );
        assert_eq!(
            RgbaColor::parse("hsv(344, 100%, 100%, 0.5)").unwrap(),
            RgbaColor::new(255, 0, 68, 128)
        );
    }

    #[test]
    fn hsv_and_hsl_round_trip_rgba() {
        let color = RgbaColor::new(18, 160, 255, 128);
        assert_eq!(RgbaColor::from_hsv(color.to_hsv()), color);
        assert_eq!(RgbaColor::from_hsl(color.to_hsl()), color);
    }

    #[test]
    fn black_has_stable_cmyk_representation() {
        let cmyk = RgbaColor::BLACK.to_cmyk();
        assert_eq!((cmyk.cyan, cmyk.magenta, cmyk.yellow), (0.0, 0.0, 0.0));
        assert_eq!(cmyk.key, 1.0);
    }

    #[test]
    fn every_display_format_preserves_alpha_information() {
        let color = RgbaColor::new(18, 160, 255, 128);
        assert!(color.display_hex().ends_with("80"));
        assert!(color.format_rgb().contains("50%"));
        assert!(color.format_hsv().contains("50%"));
        assert!(color.format_hsl().contains("50%"));
        assert!(color.format_cmyk().contains("50%"));
    }
}
