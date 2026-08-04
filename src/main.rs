use folder_diff::{app, config::AppConfig};
use std::{env, ffi::OsString, path::PathBuf, process};

#[derive(Debug, Default)]
struct Arguments {
    choose: bool,
    left: Option<PathBuf>,
    right: Option<PathBuf>,
}

impl Arguments {
    fn parse() -> Result<Self, String> {
        let mut arguments = Self::default();
        let mut values = env::args_os().skip(1);
        while let Some(value) = values.next() {
            match value.to_str() {
                Some("--choose") => arguments.choose = true,
                Some("--left") => arguments.left = Some(required_path(&mut values, "--left")?),
                Some("--right") => arguments.right = Some(required_path(&mut values, "--right")?),
                Some("--help" | "-h") => {
                    print_help();
                    process::exit(0);
                }
                Some("--version" | "-V") => {
                    println!("folder-diff {}", env!("CARGO_PKG_VERSION"));
                    process::exit(0);
                }
                Some(value) => return Err(format!("unknown argument: {value}")),
                None => return Err("arguments must be valid UTF-8 except for path values".into()),
            }
        }
        Ok(arguments)
    }
}

fn required_path(
    values: &mut impl Iterator<Item = OsString>,
    option: &str,
) -> Result<PathBuf, String> {
    values
        .next()
        .map(PathBuf::from)
        .ok_or_else(|| format!("{option} requires a directory path"))
}

fn print_help() {
    println!(
        "Folder Diff {}\n\nCompare and merge two directory trees in a native GPUI application.\n\nUSAGE:\n    folder-diff [--choose] [--left DIRECTORY] [--right DIRECTORY]\n\nOPTIONS:\n    --choose           Forget remembered roots and choose both directories\n    --left DIRECTORY   Directory shown on the left\n    --right DIRECTORY  Directory shown on the right\n    -h, --help         Print help\n    -V, --version      Print version",
        env!("CARGO_PKG_VERSION")
    );
}

fn main() {
    let arguments = Arguments::parse().unwrap_or_else(|error| {
        eprintln!("folder-diff: {error}\nTry 'folder-diff --help'.");
        process::exit(2);
    });
    let mut config = AppConfig::load();
    if arguments.choose {
        config.left_root = None;
        config.right_root = None;
    }
    if let Some(left) = arguments.left {
        config.left_root = Some(left.canonicalize().unwrap_or(left));
    }
    if let Some(right) = arguments.right {
        config.right_root = Some(right.canonicalize().unwrap_or(right));
    }
    app::run(config);
}
