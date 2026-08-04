use crate::{
    error::{Context, Result},
    model::CompareOptions,
    temp::atomic_write,
};
use std::{
    env, fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Default)]
pub struct AppConfig {
    pub left_root: Option<PathBuf>,
    pub right_root: Option<PathBuf>,
    pub compare: CompareOptions,
}

impl AppConfig {
    pub fn load() -> Self {
        Self::try_load().unwrap_or_default()
    }

    pub fn try_load() -> Result<Self> {
        let path = config_path()?;
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(error.into()),
        };
        let mut config = Self::default();
        let mut ignores = Vec::new();
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            match key {
                "left" => config.left_root = decode_path(value),
                "right" => config.right_root = decode_path(value),
                "ignore" => {
                    if let Some(value) = decode_utf8(value) {
                        ignores.push(value);
                    }
                }
                "ignore_whitespace" => config.compare.ignore_whitespace = value == "1",
                "ignore_line_endings" => config.compare.ignore_line_endings = value != "0",
                "show_identical" => config.compare.show_identical = value == "1",
                _ => {}
            }
        }
        if !ignores.is_empty() {
            config.compare.ignore_patterns = ignores;
        }
        Ok(config)
    }

    pub fn save(&self) -> Result<()> {
        let path = config_path()?;
        let parent = path.parent().context("configuration path has no parent")?;
        fs::create_dir_all(parent)?;
        let mut text = String::from("format=1\n");
        if let Some(path) = &self.left_root {
            text.push_str("left=");
            text.push_str(&encode_path(path));
            text.push('\n');
        }
        if let Some(path) = &self.right_root {
            text.push_str("right=");
            text.push_str(&encode_path(path));
            text.push('\n');
        }
        text.push_str(&format!(
            "ignore_whitespace={}\nignore_line_endings={}\nshow_identical={}\n",
            u8::from(self.compare.ignore_whitespace),
            u8::from(self.compare.ignore_line_endings),
            u8::from(self.compare.show_identical),
        ));
        for pattern in &self.compare.ignore_patterns {
            text.push_str("ignore=");
            text.push_str(&encode_bytes(pattern.as_bytes()));
            text.push('\n');
        }
        atomic_write(&path, text.as_bytes())
    }
}

pub fn config_path() -> Result<PathBuf> {
    let home = env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join("Library/Application Support/dev.folderdiff.Folder Diff/config"))
}

fn encode_path(path: &Path) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        encode_bytes(path.as_os_str().as_bytes())
    }
    #[cfg(not(unix))]
    {
        encode_bytes(path.to_string_lossy().as_bytes())
    }
}

fn decode_path(value: &str) -> Option<PathBuf> {
    let bytes = decode_bytes(value)?;
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt as _;
        Some(std::ffi::OsString::from_vec(bytes).into())
    }
    #[cfg(not(unix))]
    {
        String::from_utf8(bytes).ok().map(PathBuf::from)
    }
}

fn decode_utf8(value: &str) -> Option<String> {
    String::from_utf8(decode_bytes(value)?).ok()
}

fn encode_bytes(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

fn decode_bytes(value: &str) -> Option<Vec<u8>> {
    if !value.len().is_multiple_of(2) {
        return None;
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| Some((hex_digit(pair[0])? << 4) | hex_digit(pair[1])?))
        .collect()
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_encoding_round_trips() {
        let path = Path::new("/tmp/a folder/日本語");
        assert_eq!(decode_path(&encode_path(path)).as_deref(), Some(path));
    }
}
