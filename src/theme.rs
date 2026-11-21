//! Omarchy's current palette, re-read through its symlink so theme switches stay live.

use std::path::PathBuf;

use rune::{Color, color::hex};
use rune_kit::{Kit, PaletteFocus, PaletteSeed, StateColors};

pub struct Theme {
    path: Option<PathBuf>,
    source: String,
    colors: Option<toml::Table>,
    pub revision: u64,
}

impl Theme {
    pub fn new() -> Self {
        let state = std::env::var_os("XDG_STATE_HOME")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")));
        Self::at(state.map(|p| p.join("omarchy/current/theme/colors.toml")))
    }

    fn at(path: Option<PathBuf>) -> Self {
        let mut theme = Self { path, source: String::new(), colors: None, revision: 0 };
        theme.refresh();
        theme
    }

    /// Read the logical path each time, not a canonical path or an inode retained by a watcher.
    /// Missing/partially written files during a switch keep the last valid palette.
    pub fn refresh(&mut self) -> bool {
        let Some(source) = self.path.as_ref().and_then(|p| std::fs::read_to_string(p).ok()) else {
            return false;
        };
        if source == self.source {
            return false;
        }
        let Ok(colors) = source.parse::<toml::Table>() else {
            return false;
        };
        let color = |name: &str| colors.get(name).and_then(toml::Value::as_str).and_then(hex);
        if ["background", "foreground", "accent"].iter().any(|name| color(name).is_none()) {
            return false;
        }
        self.colors = Some(colors);
        self.source = source;
        self.revision = self.revision.wrapping_add(1);
        true
    }

    pub fn color(&self, name: &str) -> Option<Color> {
        self.colors.as_ref()?.get(name)?.as_str().and_then(hex)
    }

    pub fn kit(&self, system_dark: bool) -> Kit {
        let (Some(bg), Some(fg), Some(accent)) = (self.color("background"), self.color("foreground"), self.color("accent")) else {
            return Kit::neutral(system_dark);
        };
        let mut k = Kit::from_palette_focus(PaletteSeed { bg, fg, accent }, PaletteFocus::Accent);
        if let Some(deep) = self.color("dark_background") {
            k.c.bg_deep = deep;
        }
        let state = |name, fallback: StateColors| {
            let fg = self.color(name).unwrap_or(fallback.fg);
            StateColors { fg, icon: fg, bg: bg.mix(fg, 0.10), border: bg.mix(fg, 0.35) }
        };
        k.c.success = state("green", k.c.success);
        k.c.warning = state("yellow", k.c.warning);
        k.c.danger = state("red", k.c.danger);
        k.c.info = state("blue", k.c.info);
        k
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    const DARK: &str = r##"
background = "#101315"
foreground = "#cacccc"
accent = "#798186"
red = "#de6145"
green = "#9fa5a9"
yellow = "#d9dbdc"
blue = "#798186"
"##;
    const LIGHT: &str = "background = '#ffffff'\nforeground = '#202020'\naccent = '#336699'\n";

    struct Dir(PathBuf);
    impl Dir {
        fn new() -> Self {
            static ID: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!("klog-theme-{}-{}", std::process::id(), ID.fetch_add(1, Ordering::Relaxed)));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn absent_theme_uses_system_appearance() {
        let theme = Theme::at(None);
        assert_eq!(theme.kit(true).c.bg_base, Kit::neutral(true).c.bg_base);
        assert_eq!(theme.kit(false).c.bg_base, Kit::neutral(false).c.bg_base);
        assert_eq!(theme.color("green"), None);
    }

    #[test]
    fn palette_sets_surfaces_accent_states_and_appearance() {
        let dir = Dir::new();
        let path = dir.0.join("colors.toml");
        std::fs::write(&path, DARK).unwrap();
        let mut theme = Theme::at(Some(path.clone()));
        let k = theme.kit(false);
        assert!(k.dark);
        assert_eq!(k.c.bg_base, hex("#101315").unwrap());
        assert_eq!(k.c.text_base, hex("#cacccc").unwrap());
        assert_eq!(k.c.border_focus, hex("#798186").unwrap());
        assert_eq!(k.c.danger.fg, hex("#de6145").unwrap());
        assert!(!theme.refresh());
        std::fs::write(path, LIGHT).unwrap();
        assert!(theme.refresh());
        assert!(!theme.kit(true).dark);
    }

    #[test]
    fn invalid_or_missing_file_retains_last_valid_theme_and_recovers() {
        let dir = Dir::new();
        let path = dir.0.join("colors.toml");
        let mut theme = Theme::at(Some(path.clone()));
        assert_eq!(theme.revision, 0);
        for invalid in ["accent =", "background = '#ffffff'", "background = 'bad'\nforeground = '#fff'\naccent = '#fff'"] {
            std::fs::write(&path, invalid).unwrap();
            assert!(!theme.refresh());
        }
        std::fs::write(&path, DARK).unwrap();
        assert!(theme.refresh());
        std::fs::write(&path, "accent =").unwrap();
        assert!(!theme.refresh());
        std::fs::remove_file(&path).unwrap();
        assert!(!theme.refresh());
        assert_eq!(theme.kit(false).c.bg_base, hex("#101315").unwrap());
        std::fs::write(&path, LIGHT).unwrap();
        assert!(theme.refresh());
        assert_eq!(theme.revision, 2);
    }

    #[cfg(unix)]
    #[test]
    fn follows_atomic_symlink_and_file_replacement() {
        use std::os::unix::fs::symlink;
        let dir = Dir::new();
        for (name, source) in [("dark", DARK), ("light", LIGHT)] {
            std::fs::create_dir(dir.0.join(name)).unwrap();
            std::fs::write(dir.0.join(name).join("colors.toml"), source).unwrap();
        }
        symlink(dir.0.join("dark"), dir.0.join("theme")).unwrap();
        let mut theme = Theme::at(Some(dir.0.join("theme/colors.toml")));
        symlink(dir.0.join("light"), dir.0.join("next")).unwrap();
        std::fs::rename(dir.0.join("next"), dir.0.join("theme")).unwrap();
        assert!(theme.refresh());
        assert!(!theme.kit(true).dark);
        std::fs::write(dir.0.join("replacement"), DARK).unwrap();
        std::fs::rename(dir.0.join("replacement"), dir.0.join("light/colors.toml")).unwrap();
        assert!(theme.refresh());
        assert!(theme.kit(false).dark);
    }
}
