//! Runtime configuration, loaded from `betterm.toml` with defaults for any
//! missing field.

use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Monospace font path; when unset we probe well-known Windows fonts.
    pub font_path: Option<String>,
    pub font_size: f32,
    pub line_height: f32,

    pub background_image: Option<String>,
    /// See-through to the desktop: 0.0 = transparent, 1.0 = opaque.
    pub background_opacity: f32,
    pub background_color: [f32; 3],
    /// Windows 11 acrylic backdrop; pair with a low `background_opacity`.
    pub acrylic: bool,

    /// Default colors for cells with no explicit color.
    pub foreground: [u8; 3],
    pub background: [u8; 3],

    pub border: BorderConfig,

    pub shell: String,
    /// When non-empty, used as-is (overriding the automatic profile injection).
    pub shell_args: Vec<String>,
    /// Inject betterm's themed prompt without touching the user's `$PROFILE`.
    pub auto_profile: bool,
    /// System-info banner at startup (needs `auto_profile`).
    pub banner: bool,

    /// Lines of scrollback to keep (mouse-wheel history). 0 disables it.
    pub scrollback: usize,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct BorderConfig {
    pub enabled: bool,
    /// Thickness in physical pixels.
    pub width: f32,
    /// Corner rounding radius in pixels.
    pub radius: f32,
    /// Seconds for one full hue rotation.
    pub speed: f32,
    /// Color stops cycling around the frame (higher = tighter rainbow).
    pub angle_density: f32,
}

impl Default for BorderConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            width: 3.0,
            radius: 10.0,
            speed: 6.0,
            angle_density: 1.0,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            font_path: None,
            font_size: 18.0,
            line_height: 1.25,
            background_image: None,
            background_opacity: 0.85,
            background_color: [0.05, 0.05, 0.08],
            acrylic: false,
            foreground: [0xE0, 0xE0, 0xE0],
            background: [0x12, 0x12, 0x18],
            border: BorderConfig::default(),
            shell: "powershell.exe".to_string(),
            shell_args: Vec::new(),
            auto_profile: true,
            banner: true,
            scrollback: crate::terminal::DEFAULT_SCROLLBACK,
        }
    }
}

impl Config {
    /// Load the first readable `betterm.toml` from the candidate paths, falling
    /// back to defaults (logging why).
    pub fn load() -> Self {
        crate::logx::log(&format!("cwd: {:?}", std::env::current_dir()));
        for path in Self::candidate_paths() {
            match std::fs::read_to_string(&path) {
                Ok(text) => match toml::from_str::<Config>(&text) {
                    Ok(cfg) => {
                        crate::logx::log(&format!("loaded config from {}", path.display()));
                        return cfg;
                    }
                    Err(e) => crate::logx::log(&format!("bad config {}: {e}", path.display())),
                },
                Err(e) => crate::logx::log(&format!("no config at {}: {e}", path.display())),
            }
        }
        crate::logx::log("using default config");
        Config::default()
    }

    fn candidate_paths() -> Vec<PathBuf> {
        // Current dir wins (dev workflow), then next to the exe, then user-global.
        let mut v = vec![PathBuf::from("betterm.toml")];
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                v.push(dir.join("betterm.toml"));
            }
        }
        if let Ok(appdata) = std::env::var("APPDATA") {
            v.push(PathBuf::from(appdata).join("betterm").join("betterm.toml"));
        }
        v
    }

    /// Resolve the font file, probing common Windows monospace fonts when none
    /// is pinned in the config.
    pub fn resolve_font(&self) -> Option<PathBuf> {
        if let Some(p) = &self.font_path {
            let pb = PathBuf::from(p);
            if pb.exists() {
                return Some(pb);
            }
            eprintln!("[betterm] configured font not found: {p}");
        }
        const CANDIDATES: &[&str] = &[
            r"C:\Windows\Fonts\CascadiaCode.ttf",
            r"C:\Windows\Fonts\CascadiaMono.ttf",
            r"C:\Windows\Fonts\consola.ttf", // Consolas
            r"C:\Windows\Fonts\lucon.ttf",   // Lucida Console
            r"C:\Windows\Fonts\cour.ttf",    // Courier New
        ];
        CANDIDATES.iter().map(PathBuf::from).find(|p| p.exists())
    }
}
