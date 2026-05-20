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
                    self.suffixes.push((suffix.to_string(), style.to_string()));
                }
            } else if let Some(indicator) = Indicator::from_key(key) {
                if style.is_empty() || style == "0" || style == "00" {
                    self.indicators.remove(&indicator);
                } else {
                    self.indicators.insert(indicator, style.to_string());
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
            if name.ends_with(suffix)
                || name
                    .to_ascii_lowercase()
                    .ends_with(&suffix.to_ascii_lowercase())
            {
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
        let text = display_with_dir_marker(display, is_dir);
        if let Some(style) = self.style_for_metadata(classify_path, metadata, symlink_target_exists)
        {
            format!("\x1b[{style}m{text}\x1b[0m")
        } else {
            text
        }
    }
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
        let lines_limit = env_usize("FZF_HISTORY_COMPLETION_LINES", 5000);
        let max_candidates = env_usize("FZF_HISTORY_COMPLETION_MAX_CANDIDATES", 500);
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
    let lines = tail_lines(&config.pwdlog_file, config.lines_limit)?;
    let dir_abs = resolve_input_dir(&config.dir, &config.home, &config.pwd);
    let dir_prefix = with_trailing_separator(&dir_abs);
    let relative_root = !config.dir.starts_with('/') && !config.dir.starts_with('~');
    let mut seen = HashSet::new();
    let mut count = 0usize;

    for line in lines.iter().rev() {
        let Some((logged_cwd, command)) = parse_pwdlog_line(line) else {
            continue;
        };
        let logged_cwd = absolutize_existing_or_lexical(Path::new(logged_cwd));
        if logged_cwd.as_os_str().is_empty() || command.is_empty() {
            continue;
        }
        let words = scan_tokens(command);
        let post_cwd = infer_post_cwd(&logged_cwd, &words, &config.home);
        for word in words {
            if should_skip_word(&word) {
                continue;
            }
            let Some(expanded) = expand_tilde(&word, &config.home) else {
                continue;
            };
            let Some(candidate) = resolve_candidate(&expanded, &logged_cwd, post_cwd.as_deref())
            else {
                continue;
            };
            if config.kind == Kind::Dir && !candidate.is_dir {
                continue;
            }
            let compare = candidate.path.as_path();
            let compare_string = path_string(&compare);
            if !compare_string.starts_with(&dir_prefix) {
                continue;
            }
            if !config.leftover.is_empty() {
                let mut leftover_prefix = dir_prefix.clone();
                leftover_prefix.push_str(&config.leftover);
                if !compare_string.starts_with(&leftover_prefix) {
                    continue;
                }
            }
            let display = display_path(
                &compare,
                &config.home,
                &config.pwd,
                &config.display_prefix,
                relative_root,
            );
            if !seen.insert(display.clone()) {
                continue;
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
            if count >= config.max_candidates {
                return Ok(());
            }
        }
    }

    Ok(())
}

fn display_with_dir_marker(display: &str, is_dir: bool) -> String {
    let mut text = display.to_string();
    if is_dir && display != "/" && !display.ends_with('/') {
        text.push('/');
    }
    text
}

fn tail_lines(path: &Path, limit: usize) -> io::Result<Vec<String>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let mut file = File::open(path)?;
    let mut pos = file.seek(SeekFrom::End(0))?;
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut newlines = 0usize;
    while pos > 0 && newlines <= limit {
        let read_len = chunk.len().min(pos as usize);
        pos -= read_len as u64;
        file.seek(SeekFrom::Start(pos))?;
        file.read_exact(&mut chunk[..read_len])?;
        newlines += chunk[..read_len].iter().filter(|&&b| b == b'\n').count();
        let mut combined = Vec::with_capacity(read_len + buffer.len());
        combined.extend_from_slice(&chunk[..read_len]);
        combined.extend_from_slice(&buffer);
        buffer = combined;
    }
    let text = String::from_utf8_lossy(&buffer);
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    if lines.len() > limit {
        lines = lines.split_off(lines.len() - limit);
    }
    Ok(lines)
}

fn parse_pwdlog_line(line: &str) -> Option<(&str, &str)> {
    let (_, rest) = line.split_once('\t')?;
    let (logged_cwd, command) = rest.split_once('\t')?;
    Some((logged_cwd, command))
}

fn scan_tokens(command: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut chars = command.chars().peekable();
    let mut single = false;
    let mut double = false;
    while let Some(ch) = chars.next() {
        if single {
            if ch == '\'' {
                single = false;
            } else {
                current.push(ch);
            }
            continue;
        }
        if double {
            match ch {
                '"' => double = false,
                '\\' => {
                    if let Some(next) = chars.next() {
                        current.push(next);
                    }
                }
                _ => current.push(ch),
            }
            continue;
        }
        match ch {
            '\'' => single = true,
            '"' => double = true,
            '\\' => {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            ' ' | '\t' | '\n' | '\r' | ';' | '|' | '&' => {
                if !current.is_empty() {
                    tokens.push(current.clone());
                    current.clear();
                }
                if (ch == '&' || ch == '|') && chars.peek() == Some(&ch) {
                    chars.next();
                }
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
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
        assert_eq!(String::from_utf8(out).unwrap(), "\x1b[35msrc/\x1b[0m\n");
    }

    #[test]
    fn emits_ls_colors_style() {
        let root = TestRoot::new();
        let dir = root.path.join("dir");
        fs::create_dir_all(&dir).unwrap();
        let colors = LsColors::new(Some("di=35:*.txt=32"));
        assert_eq!(colors.colorize("dir", &dir, true), "\x1b[35mdir/\x1b[0m");
        let file = root.path.join("file.txt");
        fs::write(&file, b"").unwrap();
        assert_eq!(
            colors.colorize("file.txt", &file, false),
            "\x1b[32mfile.txt\x1b[0m"
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
            "\x1b[36mlink/\x1b[0m\n\x1b[31mdangling\x1b[0m\n"
        );
    }
}
