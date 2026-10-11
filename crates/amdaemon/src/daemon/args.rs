//! `amdaemon::process::ArgumentsParser` (ctor `0x1400C80C0`, parse `0x1400C8110`).
//!
//! Spec: `spec/controllers/lifecycle.md` §2. The command line is tokenised into options (tokens starting with
//! `-`, matched case-insensitively against a five-entry table) and their arguments (every other token,
//! verbatim). Nothing is assigned unless the whole command line validates.

use std::ffi::OsStr;

use crate::daemon::paths;

/// Usage text printed to stderr by `wWinMain` when [`ArgumentsParser::parse`] fails (`0x1405BCB90`). Only
/// `-c` and `-l` are advertised; `-m`, `-f` and `-w` exist but are not listed.
pub const USAGE: &str = "Usage: amdaemon.exe [-c <config file path ...>] [-l <log file path>]\n";

/// Why [`ArgumentsParser::parse`] rejected the command line.
///
/// The original also returns `1` for a null `argv` or a null token; a Rust slice cannot express either.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    /// Unknown option, argument without an option, wrong argument count or bad `-w` value (status `2`).
    #[error("invalid command line")]
    InvalidCommandLine,
    /// `GetModuleFileNameW` yielded no directory (status `4`).
    #[error("executable directory unavailable")]
    ExeDirUnavailable,
}

/// One row of the static option table (`0x1408D9D40`, 5 × `0x28` bytes).
#[derive(Debug, Clone, Copy)]
struct OptionSpec {
    key: &'static str,
    takes_args: bool,
    multi: bool,
}

/// Table indices: `-c` 0, `-l` 1, `-m` 2 (destructured in [`ArgumentsParser::parse`]), `-f` and `-w` below.
const FRAME: usize = 3;
const WINDOW: usize = 4;

/// Option table in lookup order (`0x1400C8680`).
const OPTIONS: [OptionSpec; 5] = [
    OptionSpec { key: "-c", takes_args: true, multi: true },
    OptionSpec { key: "-l", takes_args: true, multi: false },
    OptionSpec { key: "-m", takes_args: true, multi: false },
    OptionSpec { key: "-f", takes_args: false, multi: false },
    OptionSpec { key: "-w", takes_args: true, multi: false },
];

/// The `std::map<wstring, vector<wstring>>` of the tokenizer: accumulated arguments per option.
#[derive(Debug, Default)]
struct Collected {
    present: [bool; 5],
    values: [Vec<String>; 5],
}

impl Collected {
    /// Per-option validation `0x1400C88D0` for the option `index` closed with `args`.
    fn close(&mut self, index: usize, args: Vec<String>) -> Result<(), ParseError> {
        let spec = OPTIONS[index];
        if spec.takes_args == args.is_empty() {
            return Err(ParseError::InvalidCommandLine);
        }
        self.present[index] = true;
        self.values[index].extend(args);
        if !spec.multi && self.values[index].len() >= 2 {
            return Err(ParseError::InvalidCommandLine);
        }
        Ok(())
    }
}

/// Parsed command line (`sizeof` = `0x80`, `0x7B` bytes used).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ArgumentsParser {
    config_paths: Vec<String>,
    exe_dir: String,
    log_path: String,
    merged_dump_path: String,
    frame_high: bool,
    window_option_given: bool,
    show_console: bool,
}

impl ArgumentsParser {
    /// Constructor `0x1400C80C0`: everything empty or `false`.
    pub fn new() -> Self {
        Self::default()
    }

    /// `parse(argc, argv)` `0x1400C8110`. `argv[0]` is skipped; `module_path` is what
    /// `GetModuleFileNameW(NULL)` returns (the directory of the running module becomes
    /// [`Self::exe_dir`]).
    ///
    /// On error the parser is left unchanged: assignment only happens after the whole command line validated.
    pub fn parse<I, S>(&mut self, argv: I, module_path: &str) -> Result<(), ParseError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let collected = Self::tokenize(argv)?;

        let exe_dir = paths::directory_with_separator(module_path);
        if exe_dir.is_empty() {
            return Err(ParseError::ExeDirUnavailable);
        }

        let window = if collected.present[WINDOW] {
            let value = collected.values[WINDOW][0].to_ascii_lowercase();
            match value.as_str() {
                "show" => Some(true),
                "hide" => Some(false),
                _ => return Err(ParseError::InvalidCommandLine),
            }
        } else {
            None
        };

        let [config, log, merged, ..] = collected.values;
        self.window_option_given = window.is_some();
        if let Some(show) = window {
            self.show_console = show;
        }
        self.config_paths = config;
        self.exe_dir = exe_dir.to_owned();
        self.log_path = log.into_iter().next().unwrap_or_default();
        self.merged_dump_path = merged.into_iter().next().unwrap_or_default();
        self.frame_high = collected.present[FRAME];
        Ok(())
    }

    /// Tokenizer `0x1400C8980`: options open on a leading `-`, every other token is an argument.
    fn tokenize<I, S>(argv: I) -> Result<Collected, ParseError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut collected = Collected::default();
        let mut open: Option<(usize, Vec<String>)> = None;
        for token in argv.into_iter().skip(1) {
            let token = token.as_ref().to_string_lossy();
            if token.starts_with('-') {
                if let Some((index, args)) = open.take() {
                    collected.close(index, args)?;
                }
                let lowered = token.to_ascii_lowercase();
                let index =
                    OPTIONS.iter().position(|spec| spec.key == lowered).ok_or(ParseError::InvalidCommandLine)?;
                open = Some((index, Vec::new()));
            } else {
                let (_, args) = open.as_mut().ok_or(ParseError::InvalidCommandLine)?;
                args.push(token.into_owned());
            }
        }
        if let Some((index, args)) = open {
            collected.close(index, args)?;
        }
        Ok(collected)
    }

    /// `-c` values (`+0x00`), in command-line order across repeated options.
    pub fn config_paths(&self) -> &[String] {
        &self.config_paths
    }

    /// Directory of the running module with its trailing separator (`+0x18`); empty before a successful parse.
    pub fn exe_dir(&self) -> &str {
        &self.exe_dir
    }

    /// `-l` value (`+0x38`).
    pub fn log_path(&self) -> &str {
        &self.log_path
    }

    /// `-m` value (`+0x58`): path of the merged-config JSON dump.
    pub fn merged_dump_path(&self) -> &str {
        &self.merged_dump_path
    }

    /// `-f` given (`+0x78`): 120 Hz instead of 66 Hz.
    pub fn frame_high(&self) -> bool {
        self.frame_high
    }

    /// `-w` given with a valid value (`+0x79`).
    pub fn window_option_given(&self) -> bool {
        self.window_option_given
    }

    /// `-w show` (`+0x7A`).
    pub fn show_console(&self) -> bool {
        self.show_console
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODULE: &str = r"C:\amd\amdaemon.exe";

    fn parse(args: &[&str]) -> Result<ArgumentsParser, ParseError> {
        let mut parser = ArgumentsParser::new();
        let mut argv = vec!["amdaemon.exe"];
        argv.extend_from_slice(args);
        parser.parse(argv, MODULE).map(|()| parser)
    }

    #[test]
    fn full_command_line_fills_every_field() {
        let parser =
            parse(&["-c", "a.json", "b.json", "-f", "-w", "hide", "-l", r"D:\log\x.log", "-m", "m.json"]).unwrap();
        assert_eq!(parser.config_paths(), ["a.json", "b.json"]);
        assert!(parser.frame_high());
        assert!(parser.window_option_given());
        assert!(!parser.show_console());
        assert_eq!(parser.log_path(), r"D:\log\x.log");
        assert_eq!(parser.merged_dump_path(), "m.json");
    }

    #[test]
    fn repeated_config_option_accumulates_in_order() {
        let parser = parse(&["-c", "a", "-c", "b", "c"]).unwrap();
        assert_eq!(parser.config_paths(), ["a", "b", "c"]);
    }

    #[test]
    fn options_are_case_insensitive_but_arguments_keep_their_case() {
        let parser = parse(&["-C", "Config.JSON", "-W", "SHOW"]).unwrap();
        assert_eq!(parser.config_paths(), ["Config.JSON"]);
        assert!(parser.window_option_given());
        assert!(parser.show_console());
    }

    #[test]
    fn rejected_command_lines() {
        for args in [
            &["-w", "maybe"][..],
            &["-f", "x"],
            &["-l"],
            &["-c"],
            &["-l", "a", "-l", "b"],
            &["-l", "a", "b"],
            &["-w", "show", "-w", "hide"],
            &["foo"],
            &["-"],
            &["--config", "a"],
            &["-x"],
            &["-c", "a", "-z"],
        ] {
            assert_eq!(parse(args).unwrap_err(), ParseError::InvalidCommandLine, "{args:?}");
        }
    }
}
