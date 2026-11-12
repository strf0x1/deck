//! Window-local font trials and bounded text scaling. Output and editable
//! values have separate families so a masking font never conceals an input.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum InterfaceFont {
    #[default]
    Adwaita,
    System,
}

impl InterfaceFont {
    pub const ALL: [Self; 2] = [Self::Adwaita, Self::System];

    pub fn family(self) -> &'static str {
        match self {
            Self::Adwaita => "Adwaita Sans",
            Self::System => ".SystemUIFont",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Adwaita => "Adwaita Sans",
            Self::System => "System UI",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CodeFont {
    #[default]
    JetBrains,
    Geist,
    Proto,
    Obel,
    ObelMasked,
}

impl CodeFont {
    pub const ALL: [Self; 5] = [Self::JetBrains, Self::Geist, Self::Proto, Self::Obel, Self::ObelMasked];

    pub fn family(self) -> &'static str {
        match self {
            Self::JetBrains => "JetBrains Mono",
            Self::Geist => "GeistMono Nerd Font",
            Self::Proto => "0xProto Nerd Font",
            Self::Obel => "Obel Mono",
            Self::ObelMasked => "Obel Mono Masked",
        }
    }

    pub fn value_family(self) -> &'static str {
        if self == Self::ObelMasked { Self::Obel.family() } else { self.family() }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::JetBrains => "JetBrains",
            Self::Geist => "Geist",
            Self::Proto => "0xProto",
            Self::Obel => "Obel",
            Self::ObelMasked => "Obel Masked",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Resize {
    Larger,
    Smaller,
    Reset,
}

#[derive(Clone, Debug, Default)]
pub struct Typography {
    pub interface: InterfaceFont,
    pub code: CodeFont,
    step: i8,
}

impl Typography {
    pub fn resize(&mut self, change: Resize) {
        self.step = match change {
            Resize::Larger => (self.step + 1).min(4),
            Resize::Smaller => (self.step - 1).max(-2),
            Resize::Reset => 0,
        };
    }

    pub fn percent(&self) -> u16 {
        (100 + i16::from(self.step) * 10) as u16
    }

    /// A rem is 14 logical pixels at the default text size.
    pub fn base_px(&self) -> f32 {
        14. * f32::from(self.percent()) / 100.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scaling_is_bounded_and_reset_keeps_the_font_trial() {
        let mut typeface = Typography { code: CodeFont::ObelMasked, ..Typography::default() };
        for _ in 0..30 {
            typeface.resize(Resize::Larger);
        }
        assert_eq!(typeface.percent(), 140);
        for _ in 0..30 {
            typeface.resize(Resize::Smaller);
        }
        assert_eq!(typeface.percent(), 80);
        typeface.resize(Resize::Reset);
        assert_eq!(typeface.percent(), 100);
        assert_eq!(typeface.code, CodeFont::ObelMasked);
    }

    #[test]
    fn editable_values_never_use_the_masked_build() {
        for font in CodeFont::ALL {
            assert_ne!(font.value_family(), "Obel Mono Masked");
        }
        assert_eq!(CodeFont::ObelMasked.value_family(), "Obel Mono");
    }
}
