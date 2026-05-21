use std::collections::{HashMap, HashSet};
use std::env;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Component, Path, PathBuf};

const LS_COLORS_DEFAULT: &str = "rs=0:lc=\x1b[:rc=m:cl=\x1b[K:ex=01;32:sg=30;43:su=37;41:di=01;34:st=37;44:ow=34;42:tw=30;42:ln=01;36:bd=01;33:cd=01;33:do=01;35:pi=33:so=01;35:";
const MISSING_COLOR: &str = "38;2;255;165;0";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Kind {
    Path,
    Dir,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum Indicator {
    Normal,
    Regular,
    Directory,
    Symlink,
    Fifo,
    Socket,
    Block,
    Char,
    Sticky,
    OtherWritable,
    StickyOtherWritable,
    Orphan,
    Executable,
    Missing,
}

impl Indicator {
    fn from_key(key: &str) -> Option<Self> {
        match key {
            "no" => Some(Self::Normal),
            "fi" => Some(Self::Regular),
            "di" => Some(Self::Directory),
            "ln" => Some(Self::Symlink),
            "pi" => Some(Self::Fifo),
            "so" => Some(Self::Socket),
            "bd" => Some(Self::Block),
            "cd" => Some(Self::Char),
            "st" => Some(Self::Sticky),
            "ow" => Some(Self::OtherWritable),
            "tw" => Some(Self::StickyOtherWritable),
            "or" => Some(Self::Orphan),
            "ex" => Some(Self::Executable),
            "mi" => Some(Self::Missing),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
struct LsColors {
    indicators: HashMap<Indicator, String>,
    suffixes: Vec<(String, String)>,
}

impl LsColors {
    fn new(env_value: Option<&str>) -> Self {
        let mut colors = Self {
            indicators: HashMap::new(),
            suffixes: Vec::new(),
        };
        colors.add_from_string(LS_COLORS_DEFAULT);
        if let Some(value) = env_value {
            colors.add_from_string(value);
        }
        colors
    }

    fn add_from_string(&mut self, input: &str) {
        for entry in input.split(':') {
            let Some((key, style)) = entry.split_once('=') else {
                continue;
            };
            if let Some(suffix) = key.strip_prefix('*') {
                if style.is_empty() || style == "0" || style == "00" {
                    self.suffixes.retain(|(existing, _)| existing != suffix);
                } else {
                    self.suffixes
                        .push((suffix.to_string(), normalize_ls_color_style(style)));
                }
            } else if let Some(indicator) = Indicator::from_key(key) {
                if style.is_empty() || style == "0" || style == "00" {
                    self.indicators.remove(&indicator);
                } else {
                    self.indicators
                        .insert(indicator, normalize_ls_color_style(style));
                }
            }
        }
    }

    fn style_for_metadata(
        &self,
        path: &Path,
        metadata: Option<&fs::Metadata>,
        symlink_target_exists: Option<bool>,
    ) -> Option<&str> {
        let indicator = indicator_for(metadata, symlink_target_exists);
        if indicator == Indicator::Missing {
            return Some(MISSING_COLOR);
        }
        if indicator == Indicator::Regular {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if let Some(style) = self.style_for_suffix(name) {
                    return Some(style);
                }
            }
        }
        self.style_for_indicator(indicator)
    }

    fn style_for_suffix(&self, name: &str) -> Option<&str> {
        let mut found = None;
        for (suffix, style) in &self.suffixes {
            if ends_with_ignore_ascii_case(name, suffix) {
                found = Some(style.as_str());
            }
        }
        found
    }

    fn style_for_indicator(&self, indicator: Indicator) -> Option<&str> {
        self.indicators
            .get(&indicator)
            .or_else(|| {
                let fallback = match indicator {
                    Indicator::Executable => Indicator::Regular,
                    Indicator::Sticky
                    | Indicator::OtherWritable
                    | Indicator::StickyOtherWritable => Indicator::Directory,
                    Indicator::Orphan | Indicator::Missing => Indicator::Symlink,
                    _ => indicator,
                };
                self.indicators.get(&fallback)
            })
            .or_else(|| self.indicators.get(&Indicator::Normal))
            .map(String::as_str)
    }

    #[cfg(test)]
    fn colorize(&self, display: &str, classify_path: &Path, is_dir: bool) -> String {
        let metadata = classify_path.symlink_metadata().ok();
        let symlink_target_exists = metadata
            .as_ref()
            .filter(|m| m.file_type().is_symlink())
            .map(|_| fs::metadata(classify_path).is_ok());
        self.colorize_with_metadata(
            display,
            classify_path,
            is_dir,
            metadata.as_ref(),
            symlink_target_exists,
        )
    }

    fn colorize_with_metadata(
        &self,
        display: &str,
        classify_path: &Path,
        is_dir: bool,
        metadata: Option<&fs::Metadata>,
        symlink_target_exists: Option<bool>,
    ) -> String {
        if metadata.is_some() {
            if let Some(output) = self.colorize_components(
                display,
                classify_path,
                is_dir,
                metadata,
                symlink_target_exists,
            ) {
                return output;
            }
        }
        self.colorize_whole_with_metadata(
            display,
            classify_path,
            is_dir,
            metadata,
            symlink_target_exists,
        )
    }

    fn colorize_whole_with_metadata(
        &self,
        display: &str,
        classify_path: &Path,
        is_dir: bool,
        metadata: Option<&fs::Metadata>,
        symlink_target_exists: Option<bool>,
    ) -> String {
        let marker = if is_dir && display != "/" && !display.ends_with('/') {
            "/"
        } else {
            ""
        };
        if let Some(style) = self.style_for_metadata(classify_path, metadata, symlink_target_exists)
        {
            format!("\x1b[{style}m{display}\x1b[0m{marker}")
        } else {
            format!("{display}{marker}")
        }
    }

    fn colorize_components(
        &self,
        display: &str,
        classify_path: &Path,
        is_dir: bool,
        metadata: Option<&fs::Metadata>,
        symlink_target_exists: Option<bool>,
    ) -> Option<String> {
        let components = display_components(display);
        if components.is_empty() {
            return None;
        }
        let marker = if is_dir && display != "/" && !display.ends_with('/') {
            "/"
        } else {
            ""
        };
        let mut current = component_base_path(display, classify_path, &components);
        let mut output = String::new();
        let last_component = components.len() - 1;

        for (index, component) in components.into_iter().enumerate() {
            match &component.path_part {
                ComponentPart::Root => current = PathBuf::from("/"),
                ComponentPart::Base => {}
                ComponentPart::Normal(name) => current.push(name),
            }
            if index == last_component {
                append_styled_component(
                    &mut output,
                    &component.text,
                    self.style_for_metadata(classify_path, metadata, symlink_target_exists),
                );
            } else {
                let component_metadata = current.symlink_metadata().ok();
                let component_symlink_target_exists = component_metadata
                    .as_ref()
                    .filter(|m| m.file_type().is_symlink())
                    .map(|_| fs::metadata(&current).is_ok());
                append_styled_component(
                    &mut output,
                    &component.text,
                    self.style_for_metadata(
                        &current,
                        component_metadata.as_ref(),
                        component_symlink_target_exists,
                    ),
                );
            }
        }

        output.push_str(marker);
        Some(output)
    }
}

fn normalize_ls_color_style(style: &str) -> String {
    let parts: Vec<_> = style.split(';').collect();
    let mut leading = Vec::new();
    let mut attrs = [false; 10];
    let mut foreground = None;
    let mut background = None;
    let mut index = 0;

    while index < parts.len() {
        let Some(code) = parse_sgr_code(parts[index]) else {
            leading.push(parts[index].to_string());
            index += 1;
            continue;
        };

        match code {
            0 => attrs = [false; 10],
            1..=5 | 7..=9 => attrs[code as usize] = true,
            6 => attrs[5] = true,
            30..=37 | 90..=97 => foreground = Some(vec![code.to_string()]),
            38 => {
                if let Some((color, consumed)) = parse_extended_color(&parts, index) {
                    foreground = Some(color);
                    index += consumed - 1;
                } else {
                    leading.push(code.to_string());
                }
            }
            40..=47 | 100..=107 => background = Some(vec![code.to_string()]),
            48 => {
                if let Some((color, consumed)) = parse_extended_color(&parts, index) {
                    background = Some(color);
                    index += consumed - 1;
                } else {
                    leading.push(code.to_string());
                }
            }
            _ => leading.push(code.to_string()),
        }
        index += 1;
    }

    let mut normalized = Vec::new();
    for attr in [1, 2, 3, 4, 5, 7, 8, 9] {
        if attrs[attr] {
            normalized.push(attr.to_string());
        }
    }
    normalized.extend(leading);
    if let Some(background) = background {
        normalized.extend(background);
    }
    if let Some(foreground) = foreground {
        normalized.extend(foreground);
    }
    normalized.join(";")
}

fn parse_sgr_code(code: &str) -> Option<u16> {
    code.parse().ok()
}

fn parse_extended_color(parts: &[&str], index: usize) -> Option<(Vec<String>, usize)> {
    let code = parse_sgr_code(parts[index])?;
    let mode = parse_sgr_code(parts.get(index + 1)?)?;
    match mode {
        5 => {
            let color = parse_sgr_code(parts.get(index + 2)?)?;
            Some((
                vec![code.to_string(), mode.to_string(), color.to_string()],
                3,
            ))
        }
        2 => {
            let red = parse_sgr_code(parts.get(index + 2)?)?;
            let green = parse_sgr_code(parts.get(index + 3)?)?;
            let blue = parse_sgr_code(parts.get(index + 4)?)?;
            Some((
                vec![
                    code.to_string(),
                    mode.to_string(),
                    red.to_string(),
                    green.to_string(),
                    blue.to_string(),
                ],
                5,
            ))
        }
        _ => None,
    }
}

#[derive(Clone, Debug)]
enum ComponentPart {
    Root,
    Base,
    Normal(String),
}

#[derive(Clone, Debug)]
struct DisplayComponent {
    text: String,
    path_part: ComponentPart,
}

fn append_styled_component(output: &mut String, text: &str, style: Option<&str>) {
    if let Some(style) = style {
        output.push_str("\x1b[");
        output.push_str(style);
        output.push('m');
        output.push_str(text);
        output.push_str("\x1b[0m");
    } else {
        output.push_str(text);
    }
}

fn display_components(display: &str) -> Vec<DisplayComponent> {
    if display == "~" {
        return vec![DisplayComponent {
            text: "~".to_string(),
            path_part: ComponentPart::Base,
        }];
    }
    if let Some(rest) = display.strip_prefix("~/") {
        let mut components = vec![DisplayComponent {
            text: "~/".to_string(),
            path_part: ComponentPart::Base,
        }];
        components.extend(normal_display_components(rest));
        return components;
    }
    let mut components = Vec::new();
    let normal_start = Path::new(display).is_absolute();
    for component in Path::new(display).components() {
        match component {
            Component::RootDir => components.push(DisplayComponent {
                text: "/".to_string(),
                path_part: ComponentPart::Root,
            }),
            Component::Normal(_) | Component::CurDir | Component::ParentDir => {
                components.push(DisplayComponent {
                    text: component.as_os_str().to_string_lossy().into_owned(),
                    path_part: ComponentPart::Normal(
                        component.as_os_str().to_string_lossy().into_owned(),
                    ),
                });
            }
            Component::Prefix(_) => return Vec::new(),
        }
    }
    add_intermediate_separators(&mut components, normal_start);
    components
}

fn normal_display_components(display: &str) -> Vec<DisplayComponent> {
    let mut components = Vec::new();
    for component in Path::new(display).components() {
        match component {
            Component::Normal(_) | Component::CurDir | Component::ParentDir => {
                components.push(DisplayComponent {
                    text: component.as_os_str().to_string_lossy().into_owned(),
                    path_part: ComponentPart::Normal(
                        component.as_os_str().to_string_lossy().into_owned(),
                    ),
                });
            }
            _ => return Vec::new(),
        }
    }
    add_intermediate_separators(&mut components, false);
    components
}

fn add_intermediate_separators(components: &mut [DisplayComponent], skip_first: bool) {
    if components.len() < 2 {
        return;
    }
    let last = components.len() - 1;
    for (index, component) in components.iter_mut().enumerate() {
        if index != last && !(skip_first && index == 0) {
            component.text.push('/');
        }
    }
}

fn component_base_path(
    display: &str,
    classify_path: &Path,
    components: &[DisplayComponent],
) -> PathBuf {
    if Path::new(display).is_absolute() {
        return PathBuf::new();
    }
    let mut base = classify_path.to_path_buf();
    for _ in components
        .iter()
        .filter(|component| matches!(component.path_part, ComponentPart::Normal(_)))
    {
        base.pop();
    }
    base
}

fn ends_with_ignore_ascii_case(name: &str, suffix: &str) -> bool {
    name.as_bytes()
        .get(name.len().saturating_sub(suffix.len())..)
        .is_some_and(|end| end.eq_ignore_ascii_case(suffix.as_bytes()))
}

fn indicator_for(
    metadata: Option<&fs::Metadata>,
    symlink_target_exists: Option<bool>,
) -> Indicator {
    let Some(metadata) = metadata else {
        return Indicator::Missing;
    };
    let file_type = metadata.file_type();
    if file_type.is_dir() {
        #[cfg(unix)]
        {
            let is_sticky = metadata.mode() & 0o1000 != 0;
            let is_other_writable = metadata.mode() & 0o002 != 0;
            if is_sticky && is_other_writable {
                return Indicator::StickyOtherWritable;
            }
            if is_other_writable {
                return Indicator::OtherWritable;
            }
            if is_sticky {
                return Indicator::Sticky;
            }
        }
        Indicator::Directory
    } else if file_type.is_symlink() {
        if symlink_target_exists.unwrap_or(false) {
            Indicator::Symlink
        } else {
            Indicator::Orphan
        }
    } else if file_type.is_file() {
        #[cfg(unix)]
        {
            if metadata.mode() & 0o111 != 0 {
                return Indicator::Executable;
            }
        }
        Indicator::Regular
    } else {
        #[cfg(unix)]
        {
            if file_type.is_fifo() {
                return Indicator::Fifo;
            }
            if file_type.is_socket() {
                return Indicator::Socket;
            }
            if file_type.is_block_device() {
                return Indicator::Block;
            }
            if file_type.is_char_device() {
                return Indicator::Char;
            }
        }
        Indicator::Missing
    }
}

#[derive(Debug)]
struct Candidate {
    path: PathBuf,
    metadata: fs::Metadata,
    symlink_target_exists: Option<bool>,
    is_dir: bool,
}

impl Candidate {
    fn from_path(path: PathBuf) -> Option<Self> {
        let metadata = path.symlink_metadata().ok()?;
        let target_metadata = metadata
            .file_type()
            .is_symlink()
            .then(|| fs::metadata(&path).ok())
            .flatten();
        let symlink_target_exists = metadata
            .file_type()
            .is_symlink()
            .then_some(target_metadata.is_some());
        let is_dir = target_metadata.as_ref().unwrap_or(&metadata).is_dir();
        Some(Self {
            path,
            metadata,
            symlink_target_exists,
            is_dir,
        })
    }
}

#[derive(Debug)]
struct Config {
    kind: Kind,
    dir: String,
    leftover: String,
    display_prefix: String,
    lines_limit: usize,
    max_candidates: usize,
    pwdlog_file: PathBuf,
    home: PathBuf,
    pwd: PathBuf,
    ls_colors: Option<LsColors>,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("historypwd: {err}");
        std::process::exit(1);
    }
}

fn run() -> io::Result<()> {
    let config = Config::from_env()?;
    let mut stdout = io::stdout().lock();
    emit_candidates(&config, &mut stdout)
}

impl Config {
    fn from_env() -> io::Result<Self> {
        let mut color = false;
        let args: Vec<_> = env::args()
            .skip(1)
            .filter(|arg| {
                if arg == "-c" || arg == "--color" {
                    color = true;
                    false
                } else {
                    true
                }
            })
            .collect();
        let mut args = args.into_iter();
        let kind = match args.next().as_deref() {
            Some("path") => Kind::Path,
            Some("dir") => Kind::Dir,
            Some(other) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("invalid kind {other:?}"),
                ));
            }
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "usage: historypwd [-c|--color] kind [dir [leftover [display_prefix]]]",
                ));
            }
        };
        let dir = args.next().unwrap_or_else(|| ".".to_string());
        let leftover = args.next().unwrap_or_default();
        let display_prefix = args.next().unwrap_or_else(|| dir.clone());
        let lines_limit = env_usize("FZF_HISTORY_COMPLETION_LINES", 3000);
        let max_candidates = env_usize("FZF_HISTORY_COMPLETION_MAX_CANDIDATES", 3000);
        let pwdlog_file = env::var_os("ZSH_PWD_HISTORY_FILE")
            .map(PathBuf::from)
            .or_else(default_pwdlog_file)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "pwdlog path is unavailable"))?;
        let home = env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        let pwd = env::var_os("PWD")
            .map(PathBuf::from)
            .unwrap_or_else(|| env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let ls_colors_env = color.then(|| env::var("LS_COLORS").ok()).flatten();
        Ok(Self {
            kind,
            dir,
            leftover,
            display_prefix,
            lines_limit,
            max_candidates,
            pwdlog_file,
            home: absolutize_existing_or_lexical(&home),
            pwd: absolutize_existing_or_lexical(&pwd),
            ls_colors: color.then(|| LsColors::new(ls_colors_env.as_deref())),
        })
    }
}

fn env_usize(name: &str, default: usize) -> usize {
    env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn default_pwdlog_file() -> Option<PathBuf> {
    let histfile = env::var_os("HISTFILE")?;
    let mut path = PathBuf::from(histfile);
    path.pop();
    path.push(".zsh_history_pwd");
    Some(path)
}

fn emit_candidates(config: &Config, out: &mut dyn Write) -> io::Result<()> {
    let dir_abs = resolve_input_dir(&config.dir, &config.home, &config.pwd);
    let dir_prefix = with_trailing_separator(&dir_abs);
    let leftover_prefix = (!config.leftover.is_empty()).then(|| {
        let mut prefix = dir_prefix.clone();
        prefix.push_str(&config.leftover);
        prefix
    });
    let relative_root = !config.dir.starts_with('/') && !config.dir.starts_with('~');
    let mut seen = HashSet::new();
    let mut last_logged_cwd: Option<(String, PathBuf)> = None;
    let mut count = 0usize;

    for_tail_lines_newest_first(&config.pwdlog_file, config.lines_limit, |line| {
        let Some((logged_cwd, command)) = parse_pwdlog_line(line) else {
            return Ok(false);
        };
        if logged_cwd.is_empty() || command.is_empty() {
            return Ok(false);
        }
        let logged_cwd = match &last_logged_cwd {
            Some((raw, path)) if raw == logged_cwd => path.clone(),
            _ => {
                let path = absolutize_existing_or_lexical(Path::new(logged_cwd));
                last_logged_cwd = Some((logged_cwd.to_string(), path.clone()));
                path
            }
        };
        let mut emit_word = |word: &str, post_cwd: Option<&Path>| -> io::Result<bool> {
            if should_skip_word(word) {
                return Ok(false);
            }
            let Some(expanded) = expand_tilde(word, &config.home) else {
                return Ok(false);
            };
            let Some(candidate) = resolve_candidate(&expanded, &logged_cwd, post_cwd) else {
                return Ok(false);
            };
            if config.kind == Kind::Dir && !candidate.is_dir {
                return Ok(false);
            }
            let compare = candidate.path.as_path();
            let compare_string = path_string(&compare);
            if !compare_string.starts_with(&dir_prefix) {
                return Ok(false);
            }
            if let Some(leftover_prefix) = &leftover_prefix
                && !compare_string.starts_with(leftover_prefix)
            {
                return Ok(false);
            }
            let display = display_path(
                &compare,
                &config.home,
                &config.pwd,
                &config.display_prefix,
                relative_root,
            );
            if !seen.insert(display.clone()) {
                return Ok(false);
            }
            let output = if let Some(ls_colors) = &config.ls_colors {
                ls_colors.colorize_with_metadata(
                    &display,
                    compare,
                    candidate.is_dir,
                    Some(&candidate.metadata),
                    candidate.symlink_target_exists,
                )
            } else {
                display_with_dir_marker(&display, candidate.is_dir)
            };
            writeln!(out, "{output}")?;
            count += 1;
            if count == 1 {
                out.flush()?;
            }
            Ok(count >= config.max_candidates)
        };

        let mut words = TokenScanner::new(command);
        let Some(first_word) = words.next() else {
            return Ok(false);
        };

        if first_word == "cd" || first_word == "pushd" {
            let words = std::iter::once(first_word).chain(words).collect::<Vec<_>>();
            let post_cwd = infer_post_cwd(&logged_cwd, &words, &config.home);
            for word in &words {
                if emit_word(word, post_cwd.as_deref())? {
                    return Ok(true);
                }
            }
        } else {
            if emit_word(&first_word, None)? {
                return Ok(true);
            }
            for word in words {
                if emit_word(&word, None)? {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    })?;

    Ok(())
}

fn display_with_dir_marker(display: &str, is_dir: bool) -> String {
    let mut text = display.to_string();
    if is_dir && display != "/" && !display.ends_with('/') {
        text.push('/');
    }
    text
}

fn for_tail_lines_newest_first(
    path: &Path,
    limit: usize,
    mut handle_line: impl FnMut(&str) -> io::Result<bool>,
) -> io::Result<()> {
    if limit == 0 {
        return Ok(());
    }
    let mut file = File::open(path)?;
    let mut pos = file.seek(SeekFrom::End(0))?;
    let mut carry = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut lines_seen = 0usize;
    let mut at_file_end = true;

    while pos > 0 && lines_seen < limit {
        let read_len = chunk.len().min(pos as usize);
        pos -= read_len as u64;
        file.seek(SeekFrom::Start(pos))?;
        file.read_exact(&mut chunk[..read_len])?;

        let mut data = Vec::with_capacity(read_len + carry.len());
        data.extend_from_slice(&chunk[..read_len]);
        data.extend_from_slice(&carry);

        let mut end = data.len();
        if at_file_end && end > 0 && data[end - 1] == b'\n' {
            end -= 1;
        }
        at_file_end = false;

        while lines_seen < limit {
            let Some(newline) = data[..end].iter().rposition(|&byte| byte == b'\n') else {
                break;
            };
            if handle_tail_line(&data[newline + 1..end], &mut handle_line)? {
                return Ok(());
            }
            lines_seen += 1;
            end = newline;
        }

        carry.clear();
        carry.extend_from_slice(&data[..end]);
    }

    if lines_seen < limit && !carry.is_empty() {
        handle_tail_line(&carry, &mut handle_line)?;
    }

    Ok(())
}

fn handle_tail_line(
    bytes: &[u8],
    handle_line: &mut impl FnMut(&str) -> io::Result<bool>,
) -> io::Result<bool> {
    let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
    match String::from_utf8_lossy(bytes) {
        std::borrow::Cow::Borrowed(line) => handle_line(line),
        std::borrow::Cow::Owned(line) => handle_line(&line),
    }
}

fn parse_pwdlog_line(line: &str) -> Option<(&str, &str)> {
    let (_, rest) = line.split_once('\t')?;
    let (logged_cwd, command) = rest.split_once('\t')?;
    Some((logged_cwd, command))
}

struct TokenScanner<'a> {
    chars: std::iter::Peekable<std::str::Chars<'a>>,
    current: String,
    single: bool,
    double: bool,
    finished: bool,
}

impl<'a> TokenScanner<'a> {
    fn new(command: &'a str) -> Self {
        Self {
            chars: command.chars().peekable(),
            current: String::new(),
            single: false,
            double: false,
            finished: false,
        }
    }
}

impl Iterator for TokenScanner<'_> {
    type Item = String;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }

        loop {
            let Some(ch) = self.chars.next() else {
                self.finished = true;
                return (!self.current.is_empty()).then(|| std::mem::take(&mut self.current));
            };

            if self.single {
                if ch == '\'' {
                    self.single = false;
                } else {
                    self.current.push(ch);
                }
                continue;
            }

            if self.double {
                match ch {
                    '"' => self.double = false,
                    '\\' => {
                        if let Some(next) = self.chars.next() {
                            self.current.push(next);
                        }
                    }
                    _ => self.current.push(ch),
                }
                continue;
            }

            match ch {
                '\'' => self.single = true,
                '"' => self.double = true,
                '\\' => {
                    if let Some(next) = self.chars.next() {
                        self.current.push(next);
                    }
                }
                ' ' | '\t' | '\n' | '\r' | ';' | '|' | '&' => {
                    if (ch == '&' || ch == '|') && self.chars.peek() == Some(&ch) {
                        self.chars.next();
                    }
                    if !self.current.is_empty() {
                        return Some(std::mem::take(&mut self.current));
                    }
                }
                _ => self.current.push(ch),
            }
        }
    }
}

#[cfg(test)]
fn scan_tokens(command: &str) -> Vec<String> {
    TokenScanner::new(command).collect()
}

fn should_skip_word(word: &str) -> bool {
    if word.is_empty() || word.starts_with('-') {
        return true;
    }
    if word.contains("$(") || word.contains('`') || word.contains("<(") || word.contains(">(") {
        return true;
    }
    if word.contains("://") || word.contains('@') && word.contains(':') {
        return true;
    }
    word.contains('=')
        && !(word.starts_with('/')
            || word.starts_with("./")
            || word.starts_with("../")
            || word.starts_with('~'))
}

fn infer_post_cwd(logged_cwd: &Path, words: &[String], home: &Path) -> Option<PathBuf> {
    let command = words.first()?;
    if command != "cd" && command != "pushd" {
        return None;
    }
    let mut cd_arg = None;
    for word in words.iter().skip(1) {
        if word == "-" {
            return None;
        }
        if word == "--" {
            continue;
        }
        if command == "pushd" && is_pushd_stack_reference(word) {
            return None;
        }
        if word.starts_with('-') {
            continue;
        }
        cd_arg = Some(word.as_str());
        break;
    }
    if command == "pushd" && cd_arg.is_none() {
        return None;
    }
    let new_cwd = match cd_arg {
        None | Some("") | Some("~") => home.to_path_buf(),
        Some(arg) if arg.starts_with("~/") => home.join(&arg[2..]),
        Some(arg) if arg.starts_with('~') => return None,
        Some(arg) if arg.starts_with('/') => PathBuf::from(arg),
        Some(arg) => logged_cwd.join(arg),
    };
    let new_cwd = absolutize_existing_or_lexical(&new_cwd);
    new_cwd.is_dir().then_some(new_cwd)
}

fn is_pushd_stack_reference(word: &str) -> bool {
    let Some(rest) = word.strip_prefix(['+', '-']) else {
        return false;
    };
    !rest.is_empty() && rest.chars().all(|ch| ch.is_ascii_digit())
}

fn expand_tilde(token: &str, home: &Path) -> Option<PathBuf> {
    if token == "~" {
        Some(home.to_path_buf())
    } else if let Some(rest) = token.strip_prefix("~/") {
        Some(home.join(rest))
    } else if token.starts_with('~') {
        None
    } else {
        Some(PathBuf::from(token))
    }
}

fn resolve_candidate(
    token: &Path,
    logged_cwd: &Path,
    post_cwd: Option<&Path>,
) -> Option<Candidate> {
    if token.is_absolute() {
        return Candidate::from_path(absolutize_lexical(token));
    }
    for cwd in [Some(logged_cwd), post_cwd].into_iter().flatten() {
        let candidate = absolutize_lexical(&cwd.join(token));
        if let Some(candidate) = Candidate::from_path(candidate) {
            return Some(candidate);
        }
    }
    None
}

fn resolve_input_dir(dir: &str, home: &Path, pwd: &Path) -> PathBuf {
    let path = if dir == "~" {
        home.to_path_buf()
    } else if let Some(rest) = dir.strip_prefix("~/") {
        home.join(rest)
    } else if dir.starts_with('/') {
        PathBuf::from(dir)
    } else {
        pwd.join(dir.strip_prefix("./").unwrap_or(dir))
    };
    absolutize_existing_or_lexical(&path)
}

fn display_path(
    path: &Path,
    home: &Path,
    pwd: &Path,
    display_prefix: &str,
    relative_root: bool,
) -> String {
    if (display_prefix == "~" || display_prefix.starts_with("~/")) && path_starts_with(path, home) {
        if path == home {
            "~".to_string()
        } else {
            format!("~/{}", path.strip_prefix(home).unwrap().display())
        }
    } else if relative_root && path_starts_with(path, pwd) && path != pwd {
        path.strip_prefix(pwd).unwrap().display().to_string()
    } else {
        path.display().to_string()
    }
}

fn path_starts_with(path: &Path, prefix: &Path) -> bool {
    path == prefix || path.starts_with(prefix)
}

fn with_trailing_separator(path: &Path) -> String {
    let mut s = path.display().to_string();
    if !s.ends_with('/') {
        s.push('/');
    }
    s
}

fn path_string(path: &Path) -> String {
    path.display().to_string()
}

fn absolutize_existing_or_lexical(path: &Path) -> PathBuf {
    if let Ok(canonical) = fs::canonicalize(path) {
        return canonical;
    }
    absolutize_lexical(path)
}

fn absolutize_lexical(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    normalize_lexical(&absolute)
}

fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            _ => out.push(component.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TestRoot {
        path: PathBuf,
    }

    impl TestRoot {
        fn new() -> Self {
            let mut path = env::temp_dir();
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            path.push(format!("historypwd-test-{unique}"));
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn scans_quoted_and_escaped_tokens() {
        assert_eq!(
            scan_tokens("vim 'space dir' two\\ words \"double quoted\" a;b && c"),
            vec![
                "vim",
                "space dir",
                "two words",
                "double quoted",
                "a",
                "b",
                "c"
            ]
        );
    }

    #[test]
    fn infers_post_cwd_for_cd_and_pushd() {
        let root = TestRoot::new();
        let cwd = root.path.join("cwd");
        let after = cwd.join("after");
        fs::create_dir_all(&after).unwrap();
        let home = root.path.join("home");
        fs::create_dir_all(&home).unwrap();
        assert_eq!(
            infer_post_cwd(&cwd, &scan_tokens("cd after && ls child"), &home),
            Some(after.clone())
        );
        assert_eq!(infer_post_cwd(&cwd, &scan_tokens("pushd +1"), &home), None);
        assert_eq!(infer_post_cwd(&cwd, &scan_tokens("cd"), &home), Some(home));
    }

    #[test]
    fn visits_tail_lines_newest_first_and_respects_limit() {
        let root = TestRoot::new();
        let pwdlog = root.path.join("pwdlog");
        fs::write(&pwdlog, "one\ntwo\nthree\nfour\n").unwrap();

        let mut lines = Vec::new();
        for_tail_lines_newest_first(&pwdlog, 3, |line| {
            lines.push(line.to_string());
            Ok(false)
        })
        .unwrap();

        assert_eq!(lines, vec!["four", "three", "two"]);
    }

    #[test]
    fn visits_tail_lines_across_chunks_and_stops_early() {
        let root = TestRoot::new();
        let pwdlog = root.path.join("pwdlog");
        let long = "x".repeat(9000);
        fs::write(
            &pwdlog,
            format!("old\n{long}\nnewest-without-trailing-newline"),
        )
        .unwrap();

        let mut lines = Vec::new();
        for_tail_lines_newest_first(&pwdlog, 10, |line| {
            lines.push(line.to_string());
            Ok(lines.len() == 2)
        })
        .unwrap();

        assert_eq!(
            lines,
            vec!["newest-without-trailing-newline".to_string(), long]
        );
    }

    #[test]
    fn filters_dedupes_and_limits_candidates() {
        let root = TestRoot::new();
        let cwd = root.path.join("cwd");
        fs::create_dir_all(cwd.join("src")).unwrap();
        fs::create_dir_all(cwd.join("aftercd/child")).unwrap();
        fs::write(cwd.join("file.txt"), b"").unwrap();
        let pwdlog = root.path.join("pwdlog");
        fs::write(
            &pwdlog,
            format!(
                "1\t{}\tvim src\n2\t{}\tcat file.txt\n3\t{}\tcd aftercd && ls child\n4\t{}\tvim src\n",
                cwd.display(),
                cwd.display(),
                cwd.display(),
                cwd.display()
            ),
        )
        .unwrap();
        let config = Config {
            kind: Kind::Dir,
            dir: ".".to_string(),
            leftover: String::new(),
            display_prefix: ".".to_string(),
            lines_limit: 5000,
            max_candidates: 2,
            pwdlog_file: pwdlog,
            home: root.path.join("home"),
            pwd: cwd,
            ls_colors: None,
        };
        let mut out = Vec::new();
        emit_candidates(&config, &mut out).unwrap();
        let output = String::from_utf8(out).unwrap();
        assert!(!output.contains("\x1b["));
        let lines: Vec<_> = output.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "src/");
        assert_eq!(lines[1], "aftercd/");
    }

    #[test]
    fn applies_leftover_and_home_display() {
        let root = TestRoot::new();
        let home = root.path.join("home");
        let project = home.join("project");
        fs::create_dir_all(&project).unwrap();
        let pwdlog = root.path.join("pwdlog");
        fs::write(
            &pwdlog,
            format!("1\t{}\tcd ~ && ls project\n", home.display()),
        )
        .unwrap();
        let config = Config {
            kind: Kind::Dir,
            dir: "~".to_string(),
            leftover: "pro".to_string(),
            display_prefix: "~".to_string(),
            lines_limit: 5000,
            max_candidates: 500,
            pwdlog_file: pwdlog,
            home: home.clone(),
            pwd: root.path.join("cwd"),
            ls_colors: None,
        };
        let mut out = Vec::new();
        emit_candidates(&config, &mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "~/project/\n");
    }

    #[test]
    fn colorizes_candidates_when_enabled() {
        let root = TestRoot::new();
        let cwd = root.path.join("cwd");
        fs::create_dir_all(cwd.join("src")).unwrap();
        let pwdlog = root.path.join("pwdlog");
        fs::write(&pwdlog, format!("1\t{}\tvim src\n", cwd.display())).unwrap();
        let config = Config {
            kind: Kind::Dir,
            dir: ".".to_string(),
            leftover: String::new(),
            display_prefix: ".".to_string(),
            lines_limit: 5000,
            max_candidates: 500,
            pwdlog_file: pwdlog,
            home: root.path.join("home"),
            pwd: cwd,
            ls_colors: Some(LsColors::new(Some("di=35"))),
        };
        let mut out = Vec::new();
        emit_candidates(&config, &mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "\x1b[35msrc\x1b[0m/\n");
    }

    #[test]
    fn colorizes_nested_relative_components_with_candidate_context() {
        let root = TestRoot::new();
        let cwd = root.path.join("cwd");
        let release = cwd.join("github/jasonyukr/historypwd/target/release");
        fs::create_dir_all(&release).unwrap();
        fs::write(release.join("historypwd.bin"), b"").unwrap();
        let pwdlog = root.path.join("pwdlog");
        fs::write(
            &pwdlog,
            format!(
                "1\t{}\tvim github/jasonyukr/historypwd/target/release/historypwd.bin\n",
                cwd.display()
            ),
        )
        .unwrap();
        let config = Config {
            kind: Kind::Path,
            dir: ".".to_string(),
            leftover: String::new(),
            display_prefix: ".".to_string(),
            lines_limit: 5000,
            max_candidates: 500,
            pwdlog_file: pwdlog,
            home: root.path.join("home"),
            pwd: cwd,
            ls_colors: Some(LsColors::new(Some("di=35:*.bin=32"))),
        };
        let mut out = Vec::new();
        emit_candidates(&config, &mut out).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "\x1b[35mgithub/\x1b[0m\x1b[35mjasonyukr/\x1b[0m\x1b[35mhistorypwd/\x1b[0m\x1b[35mtarget/\x1b[0m\x1b[35mrelease/\x1b[0m\x1b[32mhistorypwd.bin\x1b[0m\n"
        );
    }

    #[test]
    fn emits_ls_colors_style() {
        let root = TestRoot::new();
        let dir = root.path.join("dir");
        fs::create_dir_all(&dir).unwrap();
        let colors = LsColors::new(Some("di=34;01:*.txt=32"));
        assert_eq!(colors.colorize("dir", &dir, true), "\x1b[1;34mdir\x1b[0m/");
        let file = root.path.join("file.txt");
        fs::write(&file, b"").unwrap();
        assert_eq!(
            colors.colorize("file.txt", &file, false),
            "\x1b[32mfile.txt\x1b[0m"
        );
        assert_eq!(
            colors.colorize("FILE.TXT", &file, false),
            "\x1b[32mFILE.TXT\x1b[0m"
        );
    }

    #[cfg(unix)]
    #[test]
    fn emits_sticky_other_writable_directory_style_with_directory_fallback() {
        use std::os::unix::fs::PermissionsExt;

        let root = TestRoot::new();
        let dir = root.path.join("shared");
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o1777)).unwrap();

        let colors = LsColors::new(Some("tw=30;42:di=35"));
        assert_eq!(
            colors.colorize("shared", &dir, true),
            "\x1b[42;30mshared\x1b[0m/"
        );

        let fallback_colors = LsColors::new(Some("tw=0:di=35"));
        assert_eq!(
            fallback_colors.colorize("shared", &dir, true),
            "\x1b[35mshared\x1b[0m/"
        );
    }

    #[test]
    fn emits_orange_for_missing_path_colorization() {
        let root = TestRoot::new();
        let colors = LsColors::new(Some("ln=36"));
        assert_eq!(
            colors.colorize("missing", &root.path.join("missing"), false),
            "\x1b[38;2;255;165;0mmissing\x1b[0m"
        );
    }

    #[cfg(unix)]
    #[test]
    fn preserves_symlink_identity_for_candidate_colorization() {
        let root = TestRoot::new();
        let cwd = root.path.join("cwd");
        let target = cwd.join("target");
        fs::create_dir_all(&target).unwrap();
        std::os::unix::fs::symlink("target", cwd.join("link")).unwrap();
        std::os::unix::fs::symlink("missing", cwd.join("dangling")).unwrap();
        let pwdlog = root.path.join("pwdlog");
        fs::write(&pwdlog, format!("1\t{}\tlink dangling\n", cwd.display())).unwrap();
        let config = Config {
            kind: Kind::Path,
            dir: ".".to_string(),
            leftover: String::new(),
            display_prefix: ".".to_string(),
            lines_limit: 5000,
            max_candidates: 500,
            pwdlog_file: pwdlog,
            home: root.path.join("home"),
            pwd: cwd,
            ls_colors: Some(LsColors::new(Some("di=35:ln=36:or=31"))),
        };
        let mut out = Vec::new();
        emit_candidates(&config, &mut out).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "\x1b[36mlink\x1b[0m/\n\x1b[31mdangling\x1b[0m\n"
        );
    }
}
