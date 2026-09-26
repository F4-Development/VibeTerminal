//! Что умеет терминал пользователя. Спросить его напрямую нельзя, поэтому
//! смотрим на переменные окружения, которые терминалы выставляют сами.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caps {
    /// 24-битные цвета (`38;2;r;g;b`). Terminal.app их не понимает и
    /// рисует вместо цвета мусорный фон.
    pub truecolor: bool,
    /// Форма указателя мыши по OSC 22 («рука» над кнопками).
    pub pointer_shape: bool,
}

impl Caps {
    pub fn detect() -> Self {
        let var = |name: &str| std::env::var(name).unwrap_or_default();
        Self::from_env(&var("COLORTERM"), &var("TERM_PROGRAM"), &var("TERM"), &var("TERMINAL_EMULATOR"))
    }

    fn from_env(colorterm: &str, term_program: &str, term: &str, terminal_emulator: &str) -> Self {
        let truecolor = match term_program {
            "Apple_Terminal" => false,
            "iTerm.app" | "ghostty" | "WezTerm" | "vscode" | "Hyper" | "Tabby" | "rio" => true,
            _ => {
                matches!(colorterm, "truecolor" | "24bit")
                    || term.contains("kitty")
                    || term.ends_with("-direct")
                    || terminal_emulator.starts_with("JetBrains")
            }
        };
        let pointer_shape = term_program == "ghostty" || term.contains("kitty");
        Self { truecolor, pointer_shape }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apple_terminal_has_no_truecolor_even_if_colorterm_says_so() {
        let caps = Caps::from_env("truecolor", "Apple_Terminal", "xterm-256color", "");
        assert!(!caps.truecolor);
    }

    #[test]
    fn modern_terminals_have_truecolor() {
        assert!(Caps::from_env("", "iTerm.app", "xterm-256color", "").truecolor);
        assert!(Caps::from_env("", "", "xterm-256color", "JetBrains-JediTerm").truecolor);
        assert!(Caps::from_env("truecolor", "", "xterm-256color", "").truecolor);
        assert!(!Caps::from_env("", "", "xterm-256color", "").truecolor);
    }

    #[test]
    fn pointer_shapes_only_where_supported() {
        assert!(Caps::from_env("", "ghostty", "xterm-ghostty", "").pointer_shape);
        assert!(!Caps::from_env("", "Apple_Terminal", "xterm-256color", "").pointer_shape);
    }
}
