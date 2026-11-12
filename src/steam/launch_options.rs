//! What launch options, shortcut programs and MangoHud settings the model may set.
//!
//! Steam runs a game's launch options through `/bin/sh -c` when they contain
//! `%command%`, so free text there is a shell command that runs on the next
//! launch. Only this form is accepted:
//!
//! `[VAR=value …] [wrapper [its options] …] %command% [game arguments …]`
//!
//! or game arguments alone. Variables, wrappers and wrapper options come from
//! fixed lists, and no part may contain shell syntax.

use std::path::{Path, PathBuf};

/// Wrappers that take no options of their own. Each ends in `exec "$@"`, so
/// the next word is the program it runs: another wrapper or `%command%`.
const PLAIN_WRAPPERS: &[&str] = &["gamemoderun", "prime-run", "obs-gamecapture"];

/// What a gamescope option takes after it.
#[derive(Clone, Copy)]
enum Operand {
    None,
    Number,
    OneOf(&'static [&'static str]),
    /// A display connector or GPU id: letters, digits, `-`, `:`.
    Name,
}

/// The gamescope options that may be set: sizes, refresh rates, scaling and
/// switches. Options that take a path (`--stats-path`, `--cursor`,
/// `--reshade-effect`, …) are left out, as is anything else.
const GAMESCOPE_OPTIONS: &[(&[&str], Operand)] = &[
    (
        &["-W", "--output-width", "-H", "--output-height", "-w", "--nested-width", "-h", "--nested-height"],
        Operand::Number,
    ),
    (&["-r", "--nested-refresh", "-o", "--nested-unfocused-refresh", "--framerate-limit"], Operand::Number),
    (&["--sharpness", "--fsr-sharpness", "--max-scale"], Operand::Number),
    (&["-F", "--filter"], Operand::OneOf(&["linear", "nearest", "fsr", "nis", "pixel"])),
    (&["-S", "--scaler"], Operand::OneOf(&["auto", "integer", "fit", "fill", "stretch"])),
    (&["--backend"], Operand::OneOf(&["auto", "sdl", "drm", "wayland", "headless", "openvr"])),
    (&["-O", "--prefer-output", "--prefer-vk-device"], Operand::Name),
    (
        &[
            "-f",
            "--fullscreen",
            "-b",
            "--borderless",
            "-e",
            "--steam",
            "-g",
            "--grab",
            "--adaptive-sync",
            "--hdr-enabled",
            "--hdr-itm-enabled",
            "--force-grab-cursor",
            "--force-windows-fullscreen",
            "--expose-wayland",
            "--immediate-flips",
            "--rt",
            "--mangoapp",
        ],
        Operand::None,
    ),
];

/// Prefixes of the environment variables that may be set.
const ENV_PREFIXES: &[&str] = &[
    "PROTON_",
    "DXVK_",
    "VKD3D_",
    "WINE_FULLSCREEN_",
    "MANGOHUD",
    "__NV_",
    "__GLX_",
    "__GL_",
    "SDL_",
    "STEAM_LINUX_RUNTIME_",
    "RADV_",
    "AMD_VULKAN_ICD",
    "ENABLE_VKBASALT",
];

/// Variables under those prefixes that run a program or load code or a config
/// from a file.
const ENV_DENIED: &[&str] =
    &["PROTON_REMOTE_DEBUG_CMD", "SDL_DYNAMIC_API", "MANGOHUD_CONFIGFILE", "MANGOHUD_PRESETSFILE"];

const COMMAND: &str = "%command%";

/// MangoHud keys that run commands, write files or upload: `exec` runs a shell
/// command, `output_folder`/`output_file` choose where logs are written,
/// `control` opens a socket, and the upload keys send logs with curl.
const MANGOHUD_DENIED_KEYS: &[&str] =
    &["exec", "output_folder", "output_file", "control", "upload_log", "upload_logs", "permit_upload"];

/// Why `key=value` can't be a MangoHud setting, or Ok when it can. Applies to
/// config files and to the inline MANGOHUD_CONFIG variable alike.
pub fn check_mangohud_setting(key: &str, value: &str) -> Result<(), String> {
    if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(format!("{key:?} is not a MangoHud setting name"));
    }
    if MANGOHUD_DENIED_KEYS.contains(&key.to_ascii_lowercase().as_str()) {
        return Err(format!("the MangoHud setting {key} runs commands, writes files or uploads data"));
    }
    if value.chars().any(|c| c.is_control() || c == '#') {
        return Err(format!("the value of {key} can't contain line breaks, control characters or '#'"));
    }
    Ok(())
}

/// Characters a word of launch options may contain: no quotes, `$`, `;`,
/// `|`, `&`, backticks, parentheses, redirections, globs or comments.
fn plain(word: &str) -> bool {
    word.chars().all(|c| c.is_ascii_alphanumeric() || "_.,:=+/-".contains(c))
}

/// A `NAME=value` word setting an allowed variable to a value without paths.
fn env_assignment(word: &str) -> Option<Result<(), String>> {
    let (name, value) = word.split_once('=')?;
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    if !ENV_PREFIXES.iter().any(|p| name.starts_with(p)) || ENV_DENIED.contains(&name) {
        return Some(Err(format!("the environment variable {name} isn't one that may be set here")));
    }
    // No paths, absolute or relative: several variables load a library or a file from one.
    if value.starts_with('.') || !value.chars().all(|c| c.is_ascii_alphanumeric() || "_.,:=+-".contains(c)) {
        return Some(Err(format!(
            "the value of {name} may only use letters, digits and _ . , : = + -, and not start with '.'"
        )));
    }
    if name == "MANGOHUD_CONFIG" {
        for (key, value) in mangohud_config_pairs(value) {
            if let Err(why) = check_mangohud_setting(key, value) {
                return Some(Err(why));
            }
        }
    }
    Some(Ok(()))
}

/// The settings MangoHud reads from a MANGOHUD_CONFIG value. Like MangoHud
/// (overlay_params.cpp), it ends a name or a value at any of `, : ; =`, so
/// `fps=1:exec=x` is two settings. (`;` and `\` never get here: `plain`
/// refuses them.)
fn mangohud_config_pairs(value: &str) -> Vec<(&str, &str)> {
    const DELIMITERS: [char; 4] = [',', ':', ';', '='];
    let mut pairs = Vec::new();
    let mut rest = value;
    while !rest.is_empty() {
        let end = rest.find(DELIMITERS).unwrap_or(rest.len());
        let key = &rest[..end];
        rest = &rest[end..];
        let mut setting = "";
        if let Some(after) = rest.strip_prefix('=') {
            let end = after.find(DELIMITERS).unwrap_or(after.len());
            setting = &after[..end];
            rest = &after[end..];
        }
        rest = rest.strip_prefix(DELIMITERS).unwrap_or(rest);
        if !key.is_empty() {
            pairs.push((key, setting));
        }
    }
    pairs
}

fn is_number(word: &str) -> bool {
    !word.is_empty() && word.chars().all(|c| c.is_ascii_digit() || c == '.') && word.chars().any(|c| c.is_ascii_digit())
}

/// Check gamescope's options in `words`, which must end with `--`; returns how
/// many words they took, the `--` included.
fn gamescope_options(words: &[&str]) -> Result<usize, String> {
    let mut i = 0;
    while i < words.len() {
        let word = words[i];
        if word == "--" {
            return Ok(i + 1);
        }
        let (option, inline) = match word.split_once('=') {
            Some((option, value)) if option.starts_with("--") => (option, Some(value)),
            _ => (word, None),
        };
        let Some(&(_, operand)) = GAMESCOPE_OPTIONS.iter().find(|(names, _)| names.contains(&option)) else {
            return Err(format!("{word:?} is not a gamescope option that may be set here"));
        };
        i += 1;
        if matches!(operand, Operand::None) {
            if inline.is_some() {
                return Err(format!("gamescope's {option} takes no value"));
            }
            continue;
        }
        let value = match inline {
            Some(v) => v,
            None => {
                let Some(v) = words.get(i).filter(|v| **v != "--") else {
                    return Err(format!("gamescope's {option} needs a value"));
                };
                i += 1;
                v
            }
        };
        let ok = match operand {
            Operand::Number => is_number(value),
            Operand::OneOf(choices) => choices.contains(&value),
            Operand::Name => !value.is_empty() && value.chars().all(|c| c.is_ascii_alphanumeric() || "-:".contains(c)),
            Operand::None => unreachable!(),
        };
        if !ok {
            return Err(format!("{value:?} is not a value gamescope's {option} may take here"));
        }
    }
    Err("gamescope's options must end with -- before the program it runs".into())
}

/// Why `options` can't be set as launch options, or Ok when they can.
pub fn check_launch_options(options: &str) -> Result<(), String> {
    if options.chars().any(|c| c.is_control() && c != '\t') {
        return Err("launch options can't contain line breaks or control characters".into());
    }
    let words: Vec<&str> = options.split([' ', '\t']).filter(|w| !w.is_empty()).collect();
    if let Some(bad) = words.iter().find(|w| **w != COMMAND && !plain(w)) {
        return Err(format!(
            "{bad:?} contains a character that isn't allowed (only letters, digits and _ . , : = + / - are)"
        ));
    }
    let Some(command_at) = words.iter().position(|w| *w == COMMAND) else {
        // Game arguments alone.
        return Ok(());
    };
    if words[command_at + 1..].contains(&COMMAND) {
        return Err("%command% may appear only once".into());
    }

    let before = &words[..command_at];
    let mut i = 0;
    while i < before.len() {
        match env_assignment(before[i]) {
            Some(result) => result?,
            None => break,
        }
        i += 1;
    }
    while i < before.len() {
        let wrapper = before[i];
        i += 1;
        match wrapper {
            "gamescope" => i += gamescope_options(&before[i..])?,
            "mangohud" => {
                if before.get(i) == Some(&"--dlsym") {
                    i += 1;
                }
            }
            "strangle" => {
                // A frame-rate limit, optionally battery:AC (`strangle 30:60`).
                if before.get(i).is_some_and(|w| w.split(':').all(is_number)) {
                    i += 1;
                }
            }
            w if PLAIN_WRAPPERS.contains(&w) => {}
            _ => {
                return Err(format!(
                    "{wrapper:?} can't come before %command%; only VAR=value settings and these wrappers can: \
                     mangohud, gamescope, strangle, {}",
                    PLAIN_WRAPPERS.join(", ")
                ));
            }
        }
    }
    Ok(())
}

/// Programs that run whatever their arguments say (shells, interpreters,
/// loaders, terminals, privilege tools, command launchers, debuggers) or that
/// destroy or fetch files. Best effort: shortcuts have no allowlist of
/// programs, so this only keeps out the obvious ones.
const RUNNERS: &[&str] = &[
    // Shells.
    "sh",
    "bash",
    "dash",
    "zsh",
    "fish",
    "ksh",
    "mksh",
    "csh",
    "tcsh",
    "ash",
    "busybox",
    "nu",
    "xonsh",
    "elvish",
    // Interpreters and runtimes.
    "python",
    "pythonw",
    "py",
    "pyw",
    "pypy",
    "perl",
    "ruby",
    "irb",
    "node",
    "nodejs",
    "deno",
    "bun",
    "php",
    "lua",
    "luajit",
    "tclsh",
    "wish",
    "pwsh",
    "powershell",
    "cmd",
    "cscript",
    "wscript",
    "mshta",
    "rundll32",
    "regsvr32",
    "osascript",
    "java",
    "javaw",
    "dotnet",
    "mono",
    "awk",
    "gawk",
    "mawk",
    "nawk",
    "sed",
    "jq",
    "gdb",
    "lldb",
    "strace",
    "ltrace",
    "valgrind",
    "make",
    "git",
    // Programs that run Windows programs.
    "proton",
    "protontricks",
    "winetricks",
    // Terminals.
    "xterm",
    "uxterm",
    "konsole",
    "kitty",
    "alacritty",
    "foot",
    "gnome-terminal",
    "xfce4-terminal",
    "terminator",
    "tilix",
    "wezterm",
    "urxvt",
    "st",
    "ptyxis",
    "kgx",
    "lxterminal",
    "mate-terminal",
    "qterminal",
    // Privilege and sandbox tools.
    "sudo",
    "doas",
    "pkexec",
    "su",
    "run0",
    "bwrap",
    "firejail",
    "flatpak",
    "flatpak-spawn",
    "systemd-run",
    "systemctl",
    "loginctl",
    // Command launchers.
    "env",
    "xargs",
    "find",
    "nohup",
    "setsid",
    "timeout",
    "nice",
    "ionice",
    "chrt",
    "taskset",
    "script",
    "expect",
    "watch",
    "flock",
    "xdg-open",
    "gio",
    "kill",
    "pkill",
    "killall",
    "ssh",
    // File destroyers and fetchers.
    "rm",
    "rmdir",
    "dd",
    "shred",
    "truncate",
    "mv",
    "cp",
    "ln",
    "install",
    "tee",
    "rsync",
    "tar",
    "chmod",
    "chown",
    "curl",
    "wget",
];

/// Whether `file_name` is one of RUNNERS, also in versioned or suffixed forms
/// (python3.12, perl5, pythonw.exe), or a dynamic loader, wine or mkfs.
fn is_runner(file_name: &str) -> bool {
    let name = file_name.to_ascii_lowercase();
    let name = [".exe", ".com"].iter().find_map(|ext| name.strip_suffix(ext)).unwrap_or(&name);
    let stem = name.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.' || c == '-');
    RUNNERS.contains(&name)
        || RUNNERS.contains(&stem)
        || ["mkfs", "wine", "ld-linux", "ld.so", "ld-musl"].iter().any(|p| name.starts_with(p))
}

/// Characters a shortcut's program or folder may not contain: Steam builds a
/// shell command from them.
fn unsafe_path_text(path: &str) -> bool {
    path.chars().any(|c| c.is_control() || "\"$`".contains(c) || (cfg!(unix) && c == '\\'))
}

/// The folder `path` is in, or one of its parents, when anyone may write to it.
#[cfg(unix)]
fn world_writable_ancestor(path: &Path) -> Option<&Path> {
    use std::os::unix::fs::PermissionsExt;
    path.ancestors().find(|dir| std::fs::metadata(dir).is_ok_and(|m| m.permissions().mode() & 0o002 != 0))
}

/// Check `exe` as a shortcut's program and return the path to save: the
/// resolved one on Unix, so a symlink can't be repointed after the check.
/// It must be an existing executable file, given by absolute path, that
/// isn't a runner (see RUNNERS) and that nobody else may write to or swap.
pub fn check_shortcut_exe(exe: &str) -> Result<String, String> {
    let path = Path::new(exe);
    if !path.is_absolute() {
        return Err(format!("{exe:?} must be an absolute path to the program"));
    }
    if unsafe_path_text(exe) {
        return Err(format!("{exe:?} contains a quote, '$', '`', a backslash or a control character"));
    }
    let real = std::fs::canonicalize(path).map_err(|_| format!("{exe:?} doesn't exist"))?;
    let meta = std::fs::metadata(&real).map_err(|e| format!("{exe:?} can't be read: {e}"))?;
    if !meta.is_file() {
        return Err(format!("{exe:?} is not a file"));
    }
    for name in [path.file_name(), real.file_name()].into_iter().flatten() {
        if is_runner(&name.to_string_lossy()) {
            return Err(format!(
                "{exe:?} is a shell, interpreter or other program that runs whatever its arguments say"
            ));
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 == 0 {
            return Err(format!("{exe:?} is not executable"));
        }
        // Anyone could have put the file there, or swap it later.
        if let Some(dir) = world_writable_ancestor(&real) {
            return Err(format!("{exe:?} is or is inside {}, which anyone can write to", dir.display()));
        }
        let real = real.to_string_lossy().into_owned();
        if unsafe_path_text(&real) {
            return Err(format!("{exe:?} resolves to {real:?}, which contains a character that isn't allowed"));
        }
        Ok(real)
    }
    #[cfg(not(unix))]
    {
        // Only programs: no .bat, .cmd, .ps1, .vbs, .com scripts.
        if !real.extension().is_some_and(|e| e.eq_ignore_ascii_case("exe")) {
            return Err(format!("{exe:?} is not an .exe program"));
        }
        let temp = std::fs::canonicalize(std::env::temp_dir()).unwrap_or_else(|_| std::env::temp_dir());
        if real.starts_with(&temp) {
            return Err(format!("{exe:?} is inside the temp folder, which other programs write to"));
        }
        Ok(display_path(&real))
    }
}

/// A resolved Windows path as Steam and Explorer write it: without the
/// `\\?\` prefix canonicalize adds (`\\?\C:\x` → `C:\x`, `\\?\UNC\s\x` → `\\s\x`).
#[cfg(not(unix))]
fn display_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}")
    } else {
        text.strip_prefix(r"\\?\").unwrap_or(&text).to_string()
    }
}

/// Check `dir` as a shortcut's working folder and return the path to save
/// (resolved on Unix, like the program): an existing folder, given by
/// absolute path, that nobody else may write to, without characters Steam's
/// command line can't take.
pub fn check_start_dir(dir: &str) -> Result<String, String> {
    if !Path::new(dir).is_absolute() {
        return Err(format!("the start folder {dir:?} must be an absolute path"));
    }
    if unsafe_path_text(dir) {
        return Err(format!("the start folder {dir:?} contains a quote, '$', '`', a backslash or a control character"));
    }
    let real: PathBuf = std::fs::canonicalize(dir).map_err(|_| format!("the start folder {dir:?} doesn't exist"))?;
    if !real.is_dir() {
        return Err(format!("the start folder {dir:?} is not a folder"));
    }
    #[cfg(unix)]
    {
        if let Some(ww) = world_writable_ancestor(&real) {
            return Err(format!(
                "the start folder {dir:?} is or is inside {}, which anyone can write to",
                ww.display()
            ));
        }
        let real = real.to_string_lossy().into_owned();
        if unsafe_path_text(&real) {
            return Err(format!(
                "the start folder {dir:?} resolves to {real:?}, which contains a character that isn't allowed"
            ));
        }
        Ok(real)
    }
    #[cfg(not(unix))]
    Ok(display_path(&real))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_the_usual_forms() {
        for ok in [
            "",
            "-novid",
            "-novid -high +fps_max 144",
            "%command%",
            "%command% -dx12",
            "mangohud %command%",
            "mangohud --dlsym %command%",
            "PROTON_LOG=1 %command%",
            "__NV_PRIME_RENDER_OFFLOAD=1 __GLX_VENDOR_LIBRARY_NAME=nvidia %command%",
            "DXVK_HUD=fps,memory MANGOHUD=1 gamemoderun mangohud %command% -windowed",
            "MANGOHUD_CONFIG=fps,position=top-left,fps_limit=60 mangohud %command%",
            "gamescope -W 1920 -H 1080 -f -- %command%",
            "gamescope -w 1280 -F fsr --fsr-sharpness=2 -- mangohud %command%",
            "gamescope --mangoapp -r 144 --backend sdl -- gamemoderun %command%",
            "strangle 60 %command%",
            "strangle 30:60 %command%",
            "%command% -game /opt/mod",
        ] {
            assert_eq!(check_launch_options(ok), Ok(()), "{ok:?}");
        }
    }

    #[test]
    fn refuses_anything_else() {
        for bad in [
            "sh -c 'curl x|sh' %command%",
            "curl x|sh; %command%",
            "%command%; rm -rf ~",
            "%command% && evil",
            "$(evil) %command%",
            "`evil` %command%",
            "%command% > /tmp/x",
            "evil %command%",
            "/usr/bin/evil %command%",
            "LD_PRELOAD=/tmp/x.so %command%",
            "WINEDLLOVERRIDES=d3d9=n %command%",
            "PATH=/tmp %command%",
            "PROTON_REMOTE_DEBUG_CMD=xterm %command%",
            "SDL_DYNAMIC_API=libx.so %command%",
            "MANGOHUD_CONFIGFILE=/tmp/x %command%",
            "MANGOHUD_CONFIGFILE=evil.conf %command%",
            "MANGOHUD_CONFIG=exec=poweroff mangohud %command%",
            "MANGOHUD_CONFIG=fps,permit_upload=1 mangohud %command%",
            "MANGOHUD_CONFIG=fps=1:exec=poweroff mangohud %command%",
            "MANGOHUD_CONFIG=fps=1=exec=poweroff mangohud %command%",
            "MANGOHUD_CONFIG=fps:control=x mangohud %command%",
            "DXVK_CONFIG_FILE=..conf %command%",
            "mangohud evil %command%",
            "mangohud -c x %command%",
            "gamemoderun -x %command%",
            "gamemoderun PROTON_LOG=1 %command%",
            "gamescope rm -- %command%",
            "gamescope -f rm -- %command%",
            "gamescope /usr/bin/touch /home/u/pwned %command%",
            "gamescope -f %command%",
            "gamescope sh /x.sh -- %command%",
            "gamescope --stats-path=/home/u/.bashrc -- %command%",
            "gamescope --cursor /x.png -- %command%",
            "gamescope -W wide -- %command%",
            "gamescope -F evil -- %command%",
            "gamescope -f=1 -- %command%",
            "gamescope -W -- %command%",
            "strangle 60 evil %command%",
            "%command% %command%",
            "x%command%",
            "-novid\nevil",
            "~/x",
            "*",
            "# %command%",
            "-game /opt/mod %command%",
        ] {
            assert!(check_launch_options(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn splits_mangohud_config_like_mangohud() {
        assert_eq!(mangohud_config_pairs("fps,position=top-left"), [("fps", ""), ("position", "top-left")]);
        assert_eq!(mangohud_config_pairs("fps=1:exec=x"), [("fps", "1"), ("exec", "x")]);
        assert_eq!(mangohud_config_pairs("fps=1=exec=x"), [("fps", "1"), ("exec", "x")]);
        assert_eq!(mangohud_config_pairs(",,fps,"), [("fps", "")]);
        assert_eq!(mangohud_config_pairs("=exec=x"), [("x", "")]);
        assert_eq!(mangohud_config_pairs("fps,,:exec"), [("fps", ""), ("exec", "")]);
    }

    #[test]
    fn mangohud_settings_are_checked() {
        for (key, value) in [("fps", ""), ("position", "top-left"), ("font_file", "/usr/share/fonts/x.ttf")] {
            assert_eq!(check_mangohud_setting(key, value), Ok(()), "{key}");
        }
        for (key, value) in [
            ("exec", "curl x|sh"),
            ("EXEC", "x"),
            ("output_folder", "/home/u"),
            ("upload_logs", "1"),
            ("control", "mangohud"),
            ("fps\nexec", "x"),
            ("fps", "1\nexec=evil"),
            ("fps", "1 # x"),
            ("", "x"),
        ] {
            assert!(check_mangohud_setting(key, value).is_err(), "{key:?}={value:?}");
        }
    }

    #[test]
    fn recognises_runners() {
        for name in [
            "sh",
            "bash",
            "python3",
            "python3.12",
            "Python.exe",
            "pythonw.exe",
            "py.exe",
            "pypy3",
            "perl5",
            "env",
            "mkfs.ext4",
            "rm",
            "ld-linux-x86-64.so.2",
            "ld.so",
            "wine",
            "wine64",
            "wine-preloader",
            "xterm",
            "curl",
            "java",
        ] {
            assert!(is_runner(name), "{name}");
        }
        for name in ["game", "lutris", "steam", "hl2_linux", "shellshock", "retroarch", "pythonista_game"] {
            assert!(!is_runner(name), "{name}");
        }
    }

    #[test]
    #[cfg(unix)]
    fn checks_shortcut_programs() {
        use std::os::unix::fs::PermissionsExt;
        // Under target/, not /tmp: the checks look at every parent folder.
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
        std::fs::create_dir_all(&base).unwrap();
        let tmp = tempfile::tempdir_in(&base).unwrap();
        let make = |dir_mode: u32, file_mode: u32| {
            let dir = tmp.path().join(format!("d{dir_mode:o}-{file_mode:o}"));
            std::fs::create_dir(&dir).unwrap();
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(dir_mode)).unwrap();
            let game = dir.join("game");
            std::fs::write(&game, "").unwrap();
            std::fs::set_permissions(&game, std::fs::Permissions::from_mode(file_mode)).unwrap();
            game
        };
        let ok = make(0o755, 0o755);
        if world_writable_ancestor(&ok).is_some() {
            // The checkout itself sits in a folder anyone can write to; nothing to check here.
            return;
        }
        let ok_str = ok.to_str().unwrap();
        assert_eq!(check_shortcut_exe(ok_str), Ok(std::fs::canonicalize(&ok).unwrap().to_string_lossy().into()));
        assert!(check_shortcut_exe(make(0o777, 0o755).to_str().unwrap()).unwrap_err().contains("anyone can write"));
        assert!(check_shortcut_exe(make(0o755, 0o757).to_str().unwrap()).unwrap_err().contains("anyone can write"));
        assert!(check_shortcut_exe(make(0o755, 0o644).to_str().unwrap()).unwrap_err().contains("not executable"));

        // A symlink in a world-writable folder is saved as its target... unless the target is unsafe.
        let ww = tmp.path().join("ww");
        std::fs::create_dir(&ww).unwrap();
        std::fs::set_permissions(&ww, std::fs::Permissions::from_mode(0o777)).unwrap();
        std::os::unix::fs::symlink(&ok, ww.join("link")).unwrap();
        let saved = check_shortcut_exe(ww.join("link").to_str().unwrap()).unwrap();
        assert_eq!(Path::new(&saved), std::fs::canonicalize(&ok).unwrap());

        assert!(check_shortcut_exe("relative/game").is_err());
        assert!(check_shortcut_exe("/nonexistent/game").is_err());
        assert!(check_shortcut_exe("/").is_err());
        assert!(check_shortcut_exe(&format!("{ok_str}$(x)")).is_err());
        if Path::new("/bin/sh").exists() {
            assert!(check_shortcut_exe("/bin/sh").unwrap_err().contains("runs whatever"));
        }

        let parent = std::fs::canonicalize(ok.parent().unwrap()).unwrap();
        assert_eq!(check_start_dir(ok.parent().unwrap().to_str().unwrap()), Ok(parent.to_string_lossy().into()));
        assert!(check_start_dir(ww.to_str().unwrap()).is_err());
        assert!(check_start_dir("relative").is_err());
        // A link to a safe folder, from a world-writable one, is saved as its target.
        std::os::unix::fs::symlink(ok.parent().unwrap(), ww.join("dirlink")).unwrap();
        assert_eq!(check_start_dir(ww.join("dirlink").to_str().unwrap()), Ok(parent.to_string_lossy().into()));
        assert!(check_start_dir("/nonexistent").is_err());
        assert!(check_start_dir("/a\"b").is_err());
    }
}
