//! K9s-format skins, resolved by semantic component rather than palette substitution.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};

use ratatui::style::{Color, Modifier, Style};
use serde_yaml::Value;

use crate::config::ConfigPaths;

#[path = "theme_invert.rs"]
mod inversion;
pub use inversion::invert_color;

const MAX_SKIN_BYTES: u64 = 1_048_576;

/// A skin contains explicit overrides. Omitted properties keep Kux's defaults.
/// Body colors form the base; component colors and then state colors take priority.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Skin {
    colors: BTreeMap<String, Color>,
    series: BTreeMap<String, Vec<Color>>,
    components: BTreeMap<String, Style>,
}

impl Skin {
    /// Parse either a standard `k9s:` skin or its `kux:` equivalent.
    ///
    /// # Errors
    /// Returns an error for invalid YAML, skin structure, or color values.
    pub fn from_yaml(input: &str) -> Result<Self, String> {
        if input.len() as u64 > MAX_SKIN_BYTES {
            return Err("Skin exceeds the 1 MiB size limit".to_owned());
        }
        let value: Value = serde_yaml::from_str(input).map_err(|error| error.to_string())?;
        let root = value
            .as_mapping()
            .ok_or_else(|| "Skin must be a YAML mapping".to_owned())?;
        let skin = root
            .get("kux")
            .or_else(|| root.get("k9s"))
            .ok_or_else(|| "Skin requires a k9s: or kux: mapping".to_owned())?;
        if !skin.is_mapping() {
            return Err("Skin root must be a mapping".to_owned());
        }
        let mut result = Self::default();
        result.read_mapping("", skin)?;
        Ok(result)
    }

    fn read_mapping(&mut self, path: &str, value: &Value) -> Result<(), String> {
        let map = value
            .as_mapping()
            .ok_or_else(|| format!("{path} must be a mapping"))?;
        for (key, value) in map {
            let key = key
                .as_str()
                .ok_or_else(|| format!("Skin key in {path} must be a string"))?;
            let child = if path.is_empty() {
                key.to_owned()
            } else {
                format!("{path}.{key}")
            };
            if matches!(
                key,
                "body"
                    | "prompt"
                    | "help"
                    | "frame"
                    | "info"
                    | "views"
                    | "dialog"
                    | "border"
                    | "title"
                    | "menu"
                    | "status"
                    | "crumbs"
                    | "table"
                    | "xray"
                    | "charts"
                    | "yaml"
                    | "picker"
                    | "logs"
                    | "indicator"
                    | "header"
                    | "resourceColors"
            ) && !value.is_mapping()
            {
                return Err(format!("{child} must be a mapping"));
            }
            if (key.contains("Color") && !key.ends_with("Colors")) || path == "prompt.border" {
                // Empty strings in upstream partial skins behave like an omitted override.
                let text = value
                    .as_str()
                    .ok_or_else(|| format!("{child} must be a color string"))?;
                if !text.trim().is_empty() {
                    let color = parse_color(text).map_err(|error| format!("{child}: {error}"))?;
                    self.colors.insert(child.clone(), color);
                    if key == "fgColor" || key == "bgColor" {
                        let component = self.components.entry(path.to_owned()).or_default();
                        *component = if key == "fgColor" {
                            component.fg(color)
                        } else {
                            component.bg(color)
                        };
                    }
                }
            } else if value.is_mapping() {
                self.read_mapping(&child, value)?;
            } else if key.ends_with("Colors") || path == "views.charts.resourceColors" {
                let entries = value
                    .as_sequence()
                    .ok_or_else(|| format!("{child} must be a list of colors"))?;
                let colors = entries
                    .iter()
                    .map(|entry| {
                        let text = entry
                            .as_str()
                            .ok_or_else(|| format!("{child} entries must be color strings"))?;
                        parse_color(text).map_err(|error| format!("{child}: {error}"))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                self.series.insert(child, colors);
            } else if key.ends_with("Style") {
                let text = value
                    .as_str()
                    .ok_or_else(|| format!("{child} must be a text style"))?;
                let modifier = match text {
                    "" | "normal" => Modifier::empty(),
                    "bold" => Modifier::BOLD,
                    "dim" => Modifier::DIM,
                    _ => return Err(format!("{child}: expected normal, bold, or dim")),
                };
                let component = self.components.entry(path.to_owned()).or_default();
                *component = component
                    .remove_modifier(Modifier::BOLD | Modifier::DIM)
                    .add_modifier(modifier);
            }
        }
        Ok(())
    }

    /// Paths in lookup priority order: user configuration, user data, system data.
    ///
    /// # Errors
    /// Rejects paths and traversal in a skin name.
    pub fn candidate_paths(paths: &ConfigPaths, name: &str) -> Result<Vec<PathBuf>, String> {
        let name = validate_name(name)?;
        let mut directories = vec![paths.config_dir.join("skins"), paths.data_dir.join("skins")];
        let system_dirs = std::env::var_os("XDG_DATA_DIRS")
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
        directories.extend(
            std::env::split_paths(&system_dirs)
                .filter(|directory| directory.is_absolute())
                .map(|directory| directory.join("kux/skins")),
        );
        let mut candidates = Vec::new();
        for directory in directories {
            for extension in ["yaml", "yml"] {
                let path = directory.join(format!("{name}.{extension}"));
                if !candidates.contains(&path) {
                    candidates.push(path);
                }
            }
        }
        Ok(candidates)
    }

    /// Load a named skin. `default` explicitly restores the built-in Kux appearance.
    ///
    /// # Errors
    /// Returns file/parse errors, including missing skins and non-regular files.
    pub fn load_named(paths: &ConfigPaths, name: &str) -> Result<Self, String> {
        if validate_name(name)? == "default" {
            return Ok(Self::default());
        }
        for path in Self::candidate_paths(paths, name)? {
            let metadata = match std::fs::metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(format!("{}: {error}", path.display())),
            };
            if !metadata.is_file() {
                return Err(format!("{}: skin must be a regular file", path.display()));
            }
            if metadata.len() > MAX_SKIN_BYTES {
                return Err(format!(
                    "{}: skin exceeds the 1 MiB size limit",
                    path.display()
                ));
            }
            return read_skin(&path);
        }
        Err(format!(
            "Skin {name:?} was not found in Kux skin directories"
        ))
    }

    /// List available named skins without reading their contents.
    #[must_use]
    pub fn names(paths: &ConfigPaths) -> Vec<String> {
        let mut names = BTreeSet::from(["default".to_owned()]);
        let mut directories = BTreeSet::new();
        for candidate in Self::candidate_paths(paths, "skin").unwrap_or_default() {
            if let Some(directory) = candidate.parent() {
                directories.insert(directory.to_owned());
            }
        }
        for directory in directories {
            let Ok(entries) = std::fs::read_dir(directory) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if !matches!(
                    path.extension().and_then(|value| value.to_str()),
                    Some("yaml" | "yml")
                ) {
                    continue;
                }
                if !entry.metadata().is_ok_and(|metadata| metadata.is_file()) {
                    continue;
                }
                if let Some(name) = path.file_stem().and_then(|value| value.to_str())
                    && validate_name(name).is_ok()
                {
                    names.insert(name.to_owned());
                }
            }
        }
        names.into_iter().collect()
    }

    #[must_use]
    pub fn color(&self, path: &str, fallback: Color) -> Color {
        self.colors.get(path).copied().unwrap_or(fallback)
    }

    #[must_use]
    pub fn color_override(&self, path: &str) -> Option<Color> {
        self.colors.get(path).copied()
    }

    #[must_use]
    pub fn colors(&self, path: &str) -> Option<&[Color]> {
        self.series.get(path).map(Vec::as_slice)
    }

    /// Apply body colors first, then explicit component foreground/background.
    #[must_use]
    pub fn style(&self, scope: &str, fallback: Style) -> Style {
        self.component_style(scope, self.component_style("body", fallback))
    }

    /// Apply only this component, useful after a parent scope has already been styled.
    #[must_use]
    pub fn component_style(&self, scope: &str, style: Style) -> Style {
        self.components
            .get(scope)
            .map_or(style, |component| style.patch(*component))
    }

    /// Color a semantic state without overwriting an omitted property's base color.
    #[must_use]
    pub fn foreground(&self, path: &str, mut style: Style) -> Style {
        if let Some(color) = self.color_override(path) {
            style = style.fg(color);
        }
        style
    }

    #[must_use]
    pub fn table_cursor(&self, fallback: Style) -> Style {
        let mut style = self.foreground("views.table.cursorFgColor", fallback);
        if let Some(color) = self.color_override("views.table.cursorBgColor") {
            style = style.bg(color);
        }
        style
    }

    #[must_use]
    pub fn dialog_button(&self, focused: bool, fallback: Style) -> Style {
        let prefix = if focused { "buttonFocus" } else { "button" };
        let mut style = self.foreground(&format!("dialog.{prefix}FgColor"), fallback);
        if let Some(color) = self.color_override(&format!("dialog.{prefix}BgColor")) {
            style = style.bg(color);
        }
        style
    }
}

fn validate_name(name: &str) -> Result<&str, String> {
    let name = name
        .strip_suffix(".yaml")
        .or_else(|| name.strip_suffix(".yml"))
        .unwrap_or(name);
    if name.is_empty()
        || matches!(name, "." | "..")
        || !name
            .chars()
            .all(|character| character.is_alphanumeric() || matches!(character, '-' | '_' | '.'))
    {
        return Err("Skin must be a name, without directory separators or traversal".to_owned());
    }
    Ok(name)
}

fn read_skin(path: &Path) -> Result<Skin, String> {
    // O_NONBLOCK prevents a concurrently replaced FIFO from blocking the reload worker.
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file: File = options
        .open(path)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    if !file
        .metadata()
        .map_err(|error| error.to_string())?
        .is_file()
    {
        return Err(format!("{}: skin must be a regular file", path.display()));
    }
    let mut input = String::new();
    file.take(MAX_SKIN_BYTES + 1)
        .read_to_string(&mut input)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    Skin::from_yaml(&input).map_err(|error| format!("{}: {error}", path.display()))
}

/// Parse terminal defaults, RGB hex, indexed ANSI colors, and CSS/tcell color names.
///
/// # Errors
/// Rejects unknown names or malformed hexadecimal/indexed colors.
pub fn parse_color(value: &str) -> Result<Color, String> {
    let value = value.trim().to_ascii_lowercase();
    if matches!(value.as_str(), "default" | "transparent" | "-") {
        return Ok(Color::Reset);
    }
    if let Some(index) = value
        .strip_prefix("color")
        .or_else(|| value.strip_prefix("ansi"))
        && let Ok(index) = index.parse::<u8>()
    {
        return Ok(Color::Indexed(index));
    }
    if let Some(hex) = value.strip_prefix('#') {
        let hex = if hex.len() == 3 {
            hex.chars()
                .flat_map(|character| [character, character])
                .collect::<String>()
        } else {
            hex.to_owned()
        };
        if hex.len() == 6
            && hex.bytes().all(|character| character.is_ascii_hexdigit())
            && let Ok(rgb) = u32::from_str_radix(&hex, 16)
        {
            return Ok(rgb_color(rgb));
        }
        return Err(format!("Invalid color {value:?}; expected #RRGGBB"));
    }
    let compact = value.replace([' ', '-', '_'], "");
    let color = CSS_COLORS
        .iter()
        .find(|(name, _)| *name == compact)
        .map(|(_, rgb)| *rgb);
    color
        .map(rgb_color)
        .ok_or_else(|| format!("Unknown skin color {value:?}"))
}

const fn rgb_color(rgb: u32) -> Color {
    Color::Rgb(
        ((rgb >> 16) & 255) as u8,
        ((rgb >> 8) & 255) as u8,
        (rgb & 255) as u8,
    )
}

// CSS names are shared by tcell's skin format. Aliases intentionally share values.
const CSS_COLORS: &[(&str, u32)] = &[
    ("aliceblue", 0x00f0_f8ff),
    ("antiquewhite", 0x00fa_ebd7),
    ("aqua", 0x0000_ffff),
    ("aquamarine", 0x007f_ffd4),
    ("azure", 0x00f0_ffff),
    ("beige", 0x00f5_f5dc),
    ("bisque", 0x00ff_e4c4),
    ("black", 0x0000_0000),
    ("blanchedalmond", 0x00ff_ebcd),
    ("blue", 0x0000_00ff),
    ("blueviolet", 0x008a_2be2),
    ("brown", 0x00a5_2a2a),
    ("burlywood", 0x00de_b887),
    ("cadetblue", 0x005f_9ea0),
    ("chartreuse", 0x007f_ff00),
    ("chocolate", 0x00d2_691e),
    ("coral", 0x00ff_7f50),
    ("cornflowerblue", 0x0064_95ed),
    ("cornsilk", 0x00ff_f8dc),
    ("crimson", 0x00dc_143c),
    ("cyan", 0x0000_ffff),
    ("darkblue", 0x0000_008b),
    ("darkcyan", 0x0000_8b8b),
    ("darkgoldenrod", 0x00b8_860b),
    ("darkgray", 0x00a9_a9a9),
    ("darkgrey", 0x00a9_a9a9),
    ("darkgreen", 0x0000_6400),
    ("darkkhaki", 0x00bd_b76b),
    ("darkmagenta", 0x008b_008b),
    ("darkolivegreen", 0x0055_6b2f),
    ("darkorange", 0x00ff_8c00),
    ("darkorchid", 0x0099_32cc),
    ("darkred", 0x008b_0000),
    ("darksalmon", 0x00e9_967a),
    ("darkseagreen", 0x008f_bc8f),
    ("darkslateblue", 0x0048_3d8b),
    ("darkslategray", 0x002f_4f4f),
    ("darkslategrey", 0x002f_4f4f),
    ("darkturquoise", 0x0000_ced1),
    ("darkviolet", 0x0094_00d3),
    ("deeppink", 0x00ff_1493),
    ("deepskyblue", 0x0000_bfff),
    ("dimgray", 0x0069_6969),
    ("dimgrey", 0x0069_6969),
    ("dodgerblue", 0x001e_90ff),
    ("firebrick", 0x00b2_2222),
    ("floralwhite", 0x00ff_faf0),
    ("forestgreen", 0x0022_8b22),
    ("fuchsia", 0x00ff_00ff),
    ("gainsboro", 0x00dc_dcdc),
    ("ghostwhite", 0x00f8_f8ff),
    ("gold", 0x00ff_d700),
    ("goldenrod", 0x00da_a520),
    ("gray", 0x0080_8080),
    ("grey", 0x0080_8080),
    ("green", 0x0000_8000),
    ("greenyellow", 0x00ad_ff2f),
    ("honeydew", 0x00f0_fff0),
    ("hotpink", 0x00ff_69b4),
    ("indianred", 0x00cd_5c5c),
    ("indigo", 0x004b_0082),
    ("ivory", 0x00ff_fff0),
    ("khaki", 0x00f0_e68c),
    ("lavender", 0x00e6_e6fa),
    ("lavenderblush", 0x00ff_f0f5),
    ("lawngreen", 0x007c_fc00),
    ("lemonchiffon", 0x00ff_facd),
    ("lightblue", 0x00ad_d8e6),
    ("lightcoral", 0x00f0_8080),
    ("lightcyan", 0x00e0_ffff),
    ("lightgoldenrodyellow", 0x00fa_fad2),
    ("lightgray", 0x00d3_d3d3),
    ("lightgrey", 0x00d3_d3d3),
    ("lightgreen", 0x0090_ee90),
    ("lightpink", 0x00ff_b6c1),
    ("lightsalmon", 0x00ff_a07a),
    ("lightseagreen", 0x0020_b2aa),
    ("lightskyblue", 0x0087_cefa),
    ("lightslategray", 0x0077_8899),
    ("lightslategrey", 0x0077_8899),
    ("lightsteelblue", 0x00b0_c4de),
    ("lightyellow", 0x00ff_ffe0),
    ("lime", 0x0000_ff00),
    ("limegreen", 0x0032_cd32),
    ("linen", 0x00fa_f0e6),
    ("magenta", 0x00ff_00ff),
    ("maroon", 0x0080_0000),
    ("mediumaquamarine", 0x0066_cdaa),
    ("mediumblue", 0x0000_00cd),
    ("mediumorchid", 0x00ba_55d3),
    ("mediumpurple", 0x0093_70db),
    ("mediumseagreen", 0x003c_b371),
    ("mediumslateblue", 0x007b_68ee),
    ("mediumspringgreen", 0x0000_fa9a),
    ("mediumturquoise", 0x0048_d1cc),
    ("mediumvioletred", 0x00c7_1585),
    ("midnightblue", 0x0019_1970),
    ("mintcream", 0x00f5_fffa),
    ("mistyrose", 0x00ff_e4e1),
    ("moccasin", 0x00ff_e4b5),
    ("navajowhite", 0x00ff_dead),
    ("navy", 0x0000_0080),
    ("oldlace", 0x00fd_f5e6),
    ("olive", 0x0080_8000),
    ("olivedrab", 0x006b_8e23),
    ("orange", 0x00ff_a500),
    ("orangered", 0x00ff_4500),
    ("orchid", 0x00da_70d6),
    ("palegoldenrod", 0x00ee_e8aa),
    ("palegreen", 0x0098_fb98),
    ("paleturquoise", 0x00af_eeee),
    ("palevioletred", 0x00db_7093),
    ("papayawhip", 0x00ff_efd5),
    ("peachpuff", 0x00ff_dab9),
    ("peru", 0x00cd_853f),
    ("pink", 0x00ff_c0cb),
    ("plum", 0x00dd_a0dd),
    ("powderblue", 0x00b0_e0e6),
    ("purple", 0x0080_0080),
    ("rebeccapurple", 0x0066_3399),
    ("red", 0x00ff_0000),
    ("rosybrown", 0x00bc_8f8f),
    ("royalblue", 0x0041_69e1),
    ("saddlebrown", 0x008b_4513),
    ("salmon", 0x00fa_8072),
    ("sandybrown", 0x00f4_a460),
    ("seagreen", 0x002e_8b57),
    ("seashell", 0x00ff_f5ee),
    ("sienna", 0x00a0_522d),
    ("silver", 0x00c0_c0c0),
    ("skyblue", 0x0087_ceeb),
    ("slateblue", 0x006a_5acd),
    ("slategray", 0x0070_8090),
    ("slategrey", 0x0070_8090),
    ("snow", 0x00ff_fafa),
    ("springgreen", 0x0000_ff7f),
    ("steelblue", 0x0046_82b4),
    ("tan", 0x00d2_b48c),
    ("teal", 0x0000_8080),
    ("thistle", 0x00d8_bfd8),
    ("tomato", 0x00ff_6347),
    ("turquoise", 0x0040_e0d0),
    ("violet", 0x00ee_82ee),
    ("wheat", 0x00f5_deb3),
    ("white", 0x00ff_ffff),
    ("whitesmoke", 0x00f5_f5f5),
    ("yellow", 0x00ff_ff00),
    ("yellowgreen", 0x009a_cd32),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_formats_match_skin_semantics() {
        assert_eq!(
            parse_color("LightSkyBlue").unwrap(),
            Color::Rgb(135, 206, 250)
        );
        assert_eq!(parse_color("#A0b1C2").unwrap(), Color::Rgb(160, 177, 194));
        assert_eq!(parse_color("#abc").unwrap(), Color::Rgb(170, 187, 204));
        assert_eq!(parse_color("color123").unwrap(), Color::Indexed(123));
        for value in ["default", "transparent", "-"] {
            assert_eq!(parse_color(value).unwrap(), Color::Reset);
        }
        for value in ["#12", "#12345g", "color256", "rainbow"] {
            assert!(parse_color(value).is_err());
        }
    }

    #[test]
    fn stock_style_groups_and_component_precedence_are_preserved() {
        let skin = Skin::from_yaml(
            r"
k9s:
  body: {fgColor: cadetblue, bgColor: black, logoColor: orange}
  prompt:
    fgColor: aqua
    border: {command: aqua, default: seagreen}
  help: {keyColor: dodgerblue, numKeyColor: fuchsia}
  frame:
    border: {fgColor: dodgerblue, focusColor: lightskyblue}
    menu: {fgColor: white, fgStyle: dim}
    status: {pendingColor: darkorange, errorColor: orangered}
  info: {fgColor: orange, sectionColor: white}
  dialog: {buttonFocusFgColor: black, buttonFocusBgColor: dodgerblue}
  views:
    table:
      fgColor: aqua
      cursorFgColor: black
      cursorBgColor: aqua
      markColor: palegreen
      header: {fgColor: white, sorterColor: aqua}
    yaml: {keyColor: steelblue, colonColor: white, valueColor: papayawhip}
    logs:
      fgColor: lightskyblue
      bgColor: default
      indicator: {toggleOnColor: limegreen, toggleOffColor: gray}
    charts:
      defaultDialColors: [palegreen, orangered]
      resourceColors: {cpu: [dodgerblue, darkslateblue]}
",
        )
        .unwrap();
        let table = skin.style("views.table", Style::default().fg(Color::Yellow));
        assert_eq!(table.fg, Some(Color::Rgb(0, 255, 255)));
        assert_eq!(table.bg, Some(Color::Rgb(0, 0, 0)));
        assert_eq!(
            skin.style("views.logs", Style::default()).bg,
            Some(Color::Reset)
        );
        assert_eq!(
            skin.color("views.yaml.valueColor", Color::White),
            Color::Rgb(255, 239, 213)
        );
        assert_eq!(
            skin.colors("views.charts.resourceColors.cpu")
                .unwrap()
                .len(),
            2
        );
        assert!(
            skin.style("frame.menu", Style::default().bold())
                .add_modifier
                .contains(Modifier::DIM)
        );
        assert!(
            !skin
                .style("frame.menu", Style::default().bold())
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert_eq!(skin.table_cursor(table).fg, Some(Color::Rgb(0, 0, 0)));
    }

    #[test]
    fn anchors_partial_skins_and_errors() {
        let skin = Skin::from_yaml("base: &base '#123456'\nk9s:\n  body: {fgColor: *base}\n  views:\n    yaml: {keyColor: ''}\n").unwrap();
        assert_eq!(
            skin.style("help", Style::default()).fg,
            Some(Color::Rgb(18, 52, 86))
        );
        assert_eq!(skin.color_override("views.yaml.keyColor"), None);
        assert!(
            Skin::from_yaml("k9s: {body: {fgColor: not-a-color}}")
                .unwrap_err()
                .contains("body.fgColor")
        );
        assert!(Skin::from_yaml("k9s: {body: {fgColor: 123}}").is_err());
        assert!(Skin::from_yaml("k9s: []").is_err());
    }

    #[test]
    fn named_skin_priority_listing_and_safe_paths() {
        let directory = std::env::temp_dir().join(format!(
            "kux-skins-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let paths = ConfigPaths::new(directory.join("config"), directory.join("data"));
        for root in [&paths.config_dir, &paths.data_dir] {
            std::fs::create_dir_all(root.join("skins")).unwrap();
        }
        std::fs::write(
            paths.data_dir.join("skins/test.yaml"),
            "k9s: {body: {fgColor: blue}}",
        )
        .unwrap();
        std::fs::write(
            paths.config_dir.join("skins/test.yml"),
            "k9s: {body: {fgColor: red}}",
        )
        .unwrap();
        assert_eq!(
            Skin::load_named(&paths, "test")
                .unwrap()
                .color_override("body.fgColor"),
            Some(Color::Rgb(255, 0, 0))
        );
        assert!(Skin::names(&paths).contains(&"test".to_owned()));
        for name in ["../test", "/tmp/test", "..", "", "foo/bar", "foo\\bar"] {
            assert!(Skin::candidate_paths(&paths, name).is_err());
        }
        std::fs::write(
            paths.config_dir.join("skins/test.yml"),
            "k9s: {body: {fgColor: bogus}}",
        )
        .unwrap();
        assert!(
            Skin::load_named(&paths, "test").is_err(),
            "invalid high-priority skin must not silently fall back"
        );
        std::fs::create_dir(paths.config_dir.join("skins/not-file.yaml")).unwrap();
        assert!(Skin::load_named(&paths, "not-file").is_err());
        let oversized = paths.config_dir.join("skins/oversized.yaml");
        File::create(&oversized)
            .unwrap()
            .set_len(MAX_SKIN_BYTES + 1)
            .unwrap();
        assert!(
            Skin::load_named(&paths, "oversized")
                .unwrap_err()
                .contains("size limit")
        );
        assert!(read_skin(&oversized).is_err());
        std::fs::remove_dir_all(directory).unwrap();
    }

    // K9s v0.51.0 skins/dracula.yaml; comments omitted from the fixture.
    const UPSTREAM_DRACULA: &str = r##"

foreground: &foreground "#f8f8f2"
background: &background "#282a36"
current_line: &current_line "#44475a"
selection: &selection "#44475a"
comment: &comment "#6272a4"
cyan: &cyan "#8be9fd"
green: &green "#50fa7b"
orange: &orange "#ffb86c"
pink: &pink "#ff79c6"
purple: &purple "#bd93f9"
red: &red "#ff5555"
yellow: &yellow "#f1fa8c"

k9s:
  body:
    fgColor: *foreground
    bgColor: *background
    logoColor: *purple
  prompt:
    fgColor: *foreground
    bgColor: *background
    suggestColor: *purple
  info:
    fgColor: *pink
    sectionColor: *foreground
  dialog:
    fgColor: *foreground
    bgColor: *background
    buttonFgColor: *foreground
    buttonBgColor: *purple
    buttonFocusFgColor: *yellow
    buttonFocusBgColor: *pink
    labelFgColor: *orange
    fieldFgColor: *foreground
  frame:
    border:
      fgColor: *selection
      focusColor: *current_line
    menu:
      fgColor: *foreground
      keyColor: *pink
      numKeyColor: *pink
    crumbs:
      fgColor: *foreground
      bgColor: *current_line
      activeColor: *current_line
    status:
      newColor: *cyan
      modifyColor: *purple
      addColor: *green
      errorColor: *red
      highlightColor: *orange
      killColor: *comment
      completedColor: *comment
    title:
      fgColor: *foreground
      bgColor: *current_line
      highlightColor: *orange
      counterColor: *purple
      filterColor: *pink
  views:
    charts:
      bgColor: default
      defaultDialColors:
        - *purple
        - *red
      defaultChartColors:
        - *purple
        - *red
    table:
      fgColor: *foreground
      bgColor: *background
      header:
        fgColor: *foreground
        bgColor: *background
        sorterColor: *cyan
    xray:
      fgColor: *foreground
      bgColor: *background
      cursorColor: *current_line
      graphicColor: *purple
      showIcons: false
    yaml:
      keyColor: *pink
      colonColor: *purple
      valueColor: *foreground
    logs:
      fgColor: *foreground
      bgColor: *background
      indicator:
        fgColor: *foreground
        bgColor: *purple
        toggleOnColor: *green
        toggleOffColor: *cyan
"##;

    #[test]
    fn upstream_dracula_skin_with_anchors_parses_without_format_changes() {
        let skin = Skin::from_yaml(UPSTREAM_DRACULA).unwrap();
        assert_eq!(
            skin.color_override("body.fgColor"),
            Some(Color::Rgb(248, 248, 242))
        );
        assert_eq!(
            skin.color_override("views.yaml.keyColor"),
            Some(Color::Rgb(255, 121, 198))
        );
        assert_eq!(
            skin.color_override("views.charts.bgColor"),
            Some(Color::Reset)
        );
        assert_eq!(
            skin.colors("views.charts.defaultDialColors"),
            Some([Color::Rgb(189, 147, 249), Color::Rgb(255, 85, 85)].as_slice())
        );
        assert_eq!(
            skin.style("views.logs", Style::default()).bg,
            Some(Color::Rgb(40, 42, 54))
        );
    }

    #[test]
    fn malformed_groups_and_color_mappings_are_rejected() {
        for yaml in [
            "k9s: {body: []}",
            "k9s: {body: {fgColor: {nested: red}}}",
            "k9s: {views: {logs: false}}",
        ] {
            assert!(Skin::from_yaml(yaml).is_err(), "{yaml}");
        }
    }
}
