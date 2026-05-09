use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::AppError;

/// 获取用户主目录，带回退和日志
///
/// ## Windows 注意事项
///
/// - `dirs::home_dir()` 在 Windows 上使用 `SHGetKnownFolderPath(FOLDERID_Profile)`，
///   返回的是真实用户目录（类似 `C:\\Users\\Alice`），与 v3.10.2 行为一致。
/// - 不要直接使用 `HOME` 环境变量：它可能由 Git/Cygwin/MSYS 等第三方工具注入，
///   且不一定等于用户目录，可能导致 `.cc-switch/cc-switch.db` 路径变化，从而“看起来像数据丢失”。
///
/// ## 测试隔离
///
/// 为了让 Windows CI/本地测试能稳定隔离真实用户数据，可通过 `CC_SWITCH_TEST_HOME`
/// 显式覆盖 home dir（仅用于测试/调试场景）。
pub fn get_home_dir() -> PathBuf {
    if let Ok(home) = std::env::var("CC_SWITCH_TEST_HOME") {
        let trimmed = home.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }

    dirs::home_dir().unwrap_or_else(|| {
        log::warn!("无法获取用户主目录，回退到当前目录");
        PathBuf::from(".")
    })
}

/// 获取 Claude Code 配置目录路径
pub fn get_claude_config_dir() -> PathBuf {
    if let Some(custom) = crate::settings::get_claude_override_dir() {
        return custom;
    }

    get_home_dir().join(".claude")
}

/// 默认 Claude MCP 配置文件路径 (~/.claude.json)
pub fn get_default_claude_mcp_path() -> PathBuf {
    get_home_dir().join(".claude.json")
}

fn derive_mcp_path_from_override(dir: &Path) -> Option<PathBuf> {
    let file_name = dir
        .file_name()
        .map(|name| name.to_string_lossy().to_string())?
        .trim()
        .to_string();
    if file_name.is_empty() {
        return None;
    }
    let parent = dir.parent().unwrap_or_else(|| Path::new(""));
    Some(parent.join(format!("{file_name}.json")))
}

/// 获取 Claude MCP 配置文件路径，若设置了目录覆盖则与覆盖目录同级
pub fn get_claude_mcp_path() -> PathBuf {
    if let Some(custom_dir) = crate::settings::get_claude_override_dir() {
        if let Some(path) = derive_mcp_path_from_override(&custom_dir) {
            return path;
        }
    }
    get_default_claude_mcp_path()
}

/// 获取 Claude Code 主配置文件路径
pub fn get_claude_settings_path() -> PathBuf {
    let dir = get_claude_config_dir();
    let settings = dir.join("settings.json");
    if settings.exists() {
        return settings;
    }
    // 兼容旧版命名：若存在旧文件则继续使用
    let legacy = dir.join("claude.json");
    if legacy.exists() {
        return legacy;
    }
    // 默认新建：回落到标准文件名 settings.json（不再生成 claude.json）
    settings
}

/// 获取应用配置目录路径 (~/.cc-switch)
pub fn get_app_config_dir() -> PathBuf {
    if let Some(custom) = crate::app_store::get_app_config_dir_override() {
        return custom;
    }

    let default_dir = get_home_dir().join(".cc-switch");

    // 兼容 v3.10.3：当用户环境存在 `HOME` 且与真实用户目录不同，
    // v3.10.3 可能在 `HOME/.cc-switch/` 下创建/使用了数据库。
    // 这里仅在“默认位置没有数据库”时回退到旧位置，避免再次出现“供应商消失”问题，
    // 同时也避免新安装因为 `HOME` 被设置而写入非预期路径。
    #[cfg(windows)]
    {
        let default_db = default_dir.join("cc-switch.db");
        if !default_db.exists() {
            if let Ok(home_env) = std::env::var("HOME") {
                let trimmed = home_env.trim();
                if !trimmed.is_empty() {
                    let legacy_dir = PathBuf::from(trimmed).join(".cc-switch");
                    if legacy_dir.join("cc-switch.db").exists() {
                        log::info!(
                            "Detected v3.10.3 legacy database at {}, using it instead of {}",
                            legacy_dir.display(),
                            default_dir.display()
                        );
                        return legacy_dir;
                    }
                }
            }
        }
    }

    default_dir
}

/// 获取应用配置文件路径
pub fn get_app_config_path() -> PathBuf {
    get_app_config_dir().join("config.json")
}

/// 清理供应商名称，确保文件名安全
#[allow(dead_code)]
pub fn sanitize_provider_name(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '-',
            _ => c,
        })
        .collect::<String>()
        .to_lowercase()
}

/// 获取供应商配置文件路径
#[allow(dead_code)]
pub fn get_provider_config_path(provider_id: &str, provider_name: Option<&str>) -> PathBuf {
    let base_name = provider_name
        .map(sanitize_provider_name)
        .unwrap_or_else(|| sanitize_provider_name(provider_id));

    get_claude_config_dir().join(format!("settings-{base_name}.json"))
}

/// 读取 JSON 配置文件
pub fn read_json_file<T: for<'a> Deserialize<'a>>(path: &Path) -> Result<T, AppError> {
    if !path.exists() {
        return Err(AppError::Config(format!("文件不存在: {}", path.display())));
    }

    let content = fs::read_to_string(path).map_err(|e| AppError::io(path, e))?;

    serde_json::from_str(&content).map_err(|e| AppError::json(path, e))
}

/// 递归排序 JSON 对象的键（按字母顺序），确保序列化输出是确定性的
fn sort_json_keys(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut sorted_map = Map::new();
            let mut keys: Vec<_> = map.keys().collect();
            keys.sort();
            for key in keys {
                sorted_map.insert(key.clone(), sort_json_keys(&map[key]));
            }
            Value::Object(sorted_map)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(sort_json_keys).collect()),
        other => other.clone(),
    }
}

/// 写入 JSON 配置文件（键按字母排序，确保确定性输出）
pub fn write_json_file<T: Serialize>(path: &Path, data: &T) -> Result<(), AppError> {
    // 确保目录存在
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| AppError::io(parent, e))?;
    }

    let value = serde_json::to_value(data).map_err(|e| AppError::JsonSerialize { source: e })?;
    let sorted_value = sort_json_keys(&value);
    let json = serde_json::to_string_pretty(&sorted_value)
        .map_err(|e| AppError::JsonSerialize { source: e })?;

    atomic_write(path, json.as_bytes())
}

/// 原子写入文本文件（用于 TOML/纯文本）
pub fn write_text_file(path: &Path, data: &str) -> Result<(), AppError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| AppError::io(parent, e))?;
    }
    atomic_write(path, data.as_bytes())
}

/// Resolve `path` through any symlink layer(s) to the real underlying file.
/// Returns `None` when `path` is already a regular file (no resolution needed)
/// so callers can use `resolved.as_deref().unwrap_or(path)` cheaply.
fn resolve_real_path(path: &Path) -> Option<PathBuf> {
    // On Windows, check WSL paths FIRST. is_symlink() returns true for ANY reparse
    // point (including IO_REPARSE_TAG_LX_SYMLINK) because FILE_ATTRIBUTE_REPARSE_POINT
    // is set, but canonicalize() cannot follow LX symlinks — only `wsl readlink` can.
    // Checking WSL paths first avoids canonicalize() swallowing the error path.
    #[cfg(windows)]
    {
        if let Some(p) = resolve_wsl_symlink(path) {
            return Some(p);
        }
    }

    // Standard POSIX / Win32 symlinks — use canonicalize for recursive resolution.
    if path.is_symlink() {
        log::debug!("atomic_write: resolving symlink: {}", path.display());
        let resolved = std::fs::canonicalize(path).ok();
        match &resolved {
            Some(r) => log::debug!("atomic_write: symlink resolved to: {}", r.display()),
            None => log::warn!(
                "atomic_write: canonicalize failed for symlink: {}",
                path.display()
            ),
        }
        return resolved;
    }

    None
}

/// Windows-only: resolve a WSL symlink on a `\\wsl.localhost\` or `\\wsl$\` path.
///
/// Runs two WSL commands: `readlink -f <path>` to canonicalize all symlink hops,
/// then `wslpath -w <canonical>` to translate the result to a Windows path.
/// Returns `None` if the path is not a WSL UNC path, is not a reparse point, or if
/// resolution fails for any reason (caller falls back to the original path).
#[cfg(windows)]
fn resolve_wsl_symlink(path: &Path) -> Option<PathBuf> {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

    let path_str = path.to_string_lossy();
    let lower = path_str.to_lowercase();

    // Only process \\wsl.localhost\ and \\wsl$\ UNC paths.
    let prefix_len = if lower.starts_with(r"\\wsl.localhost\") {
        r"\\wsl.localhost\".len()
    } else if lower.starts_with(r"\\wsl$\") {
        r"\\wsl$\".len()
    } else {
        return None;
    };

    // Bail out quickly for non-reparse-point entries (regular files inside WSL don't need resolution).
    let meta = std::fs::symlink_metadata(path).ok()?;
    if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
        log::debug!(
            "atomic_write: WSL path is a regular file, no symlink resolution needed: {}",
            path.display()
        );
        return None;
    }

    log::debug!(
        "atomic_write: WSL path has ReparsePoint attribute (LX symlink): {}",
        path.display()
    );

    // Parse: \\wsl.localhost\<distro>\<rest...>
    let after_server = &path_str[prefix_len..];
    let slash_pos = after_server.find('\\')?;
    let distro = &after_server[..slash_pos];
    // Convert Windows-style separators to forward slashes for the WSL path.
    let wsl_path = after_server[slash_pos..].replace('\\', "/");
    // wsl_path is now like /home/wangbin/.claude/settings.json

    log::debug!("atomic_write: WSL distro={distro}, wsl_path={wsl_path}");

    // Step 1: resolve all symlink hops to get the canonical WSL path.
    let readlink_out = std::process::Command::new("wsl")
        .args(["-d", distro, "--", "readlink", "-f", &wsl_path])
        .output()
        .ok()?;

    if !readlink_out.status.success() {
        log::warn!(
            "atomic_write: `wsl -d {distro} readlink -f {wsl_path}` failed (exit {}): {}",
            readlink_out.status,
            String::from_utf8_lossy(&readlink_out.stderr).trim()
        );
        return None;
    }

    let canonical_wsl = String::from_utf8(readlink_out.stdout).ok()?;
    let canonical_wsl = canonical_wsl.trim();
    if canonical_wsl.is_empty() {
        log::warn!("atomic_write: readlink -f returned empty output for {wsl_path} in {distro}");
        return None;
    }

    log::debug!("atomic_write: canonical WSL path: {canonical_wsl}");

    // Step 2: translate the canonical WSL path to a Windows path.
    // wslpath -w converts /mnt/X/... -> X:\... and returns a \\wsl$\... UNC path for WSL-only paths.
    let wslpath_out = std::process::Command::new("wsl")
        .args(["-d", distro, "--", "wslpath", "-w", canonical_wsl])
        .output()
        .ok()?;

    if !wslpath_out.status.success() {
        log::warn!(
            "atomic_write: `wsl -d {distro} wslpath -w {canonical_wsl}` failed (exit {}): {}",
            wslpath_out.status,
            String::from_utf8_lossy(&wslpath_out.stderr).trim()
        );
        return None;
    }

    let win_path = String::from_utf8(wslpath_out.stdout).ok()?;
    let win_path = win_path.trim();
    if win_path.is_empty() {
        log::warn!(
            "atomic_write: wslpath -w returned empty for canonical path {canonical_wsl} in {distro}"
        );
        return None;
    }

    log::debug!("atomic_write: resolved WSL symlink -> Windows path: {win_path}");

    Some(PathBuf::from(win_path))
}

/// 原子写入：写入临时文件后 rename 替换，避免半写状态
///
/// 如果目标路径是符号链接，会解析到真实路径再进行写入，这样 rename 操作替换的是
/// 真实文件而非符号链接本身，从而保留符号链接不变。
///
/// 在 Windows 上额外处理 WSL 符号链接（IO_REPARSE_TAG_LX_SYMLINK），这类链接的
/// reparse tag 不被 Rust 标准库的 is_symlink() 识别，需要通过 wsl 命令解析。
pub fn atomic_write(path: &Path, data: &[u8]) -> Result<(), AppError> {
    // Resolve symlinks so rename() replaces the real file, not the symlink entry.
    // On POSIX, rename() operates on the directory entry — renaming over a symlink
    // replaces the symlink itself with the new regular file.
    //
    // On Windows we also handle WSL symlinks (IO_REPARSE_TAG_LX_SYMLINK): these show
    // as ReparsePoint in file attributes but is_symlink() returns false because the tag
    // is unknown to the Win32 symlink API. We detect them via the ReparsePoint attribute
    // on \\wsl.localhost\ paths and resolve via `wsl readlink -f | wslpath -w`.
    let resolved = resolve_real_path(path);
    let path: &Path = resolved.as_deref().unwrap_or(path);

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| AppError::io(parent, e))?;
    }

    let parent = path
        .parent()
        .ok_or_else(|| AppError::Config("无效的路径".to_string()))?;
    let mut tmp = parent.to_path_buf();
    let file_name = path
        .file_name()
        .ok_or_else(|| AppError::Config("无效的文件名".to_string()))?
        .to_string_lossy()
        .to_string();
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    tmp.push(format!("{file_name}.tmp.{ts}"));

    {
        let mut f = fs::File::create(&tmp).map_err(|e| AppError::io(&tmp, e))?;
        f.write_all(data).map_err(|e| AppError::io(&tmp, e))?;
        f.flush().map_err(|e| AppError::io(&tmp, e))?;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = fs::metadata(&path) {
            let perm = meta.permissions().mode();
            let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(perm));
        }
    }

    #[cfg(windows)]
    {
        // Windows 上 rename 目标存在会失败，先移除再重命名（尽量接近原子性）
        if path.exists() {
            let _ = fs::remove_file(&path);
        }
        fs::rename(&tmp, &path).map_err(|e| AppError::IoContext {
            context: format!("原子替换失败: {} -> {}", tmp.display(), path.display()),
            source: e,
        })?;
    }

    #[cfg(not(windows))]
    {
        fs::rename(&tmp, &path).map_err(|e| AppError::IoContext {
            context: format!("原子替换失败: {} -> {}", tmp.display(), path.display()),
            source: e,
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_mcp_path_from_override_preserves_folder_name() {
        let override_dir = PathBuf::from("/tmp/profile/.claude");
        let derived = derive_mcp_path_from_override(&override_dir)
            .expect("should derive path for nested dir");
        assert_eq!(derived, PathBuf::from("/tmp/profile/.claude.json"));
    }

    #[test]
    fn derive_mcp_path_from_override_handles_non_hidden_folder() {
        let override_dir = PathBuf::from("/data/claude-config");
        let derived = derive_mcp_path_from_override(&override_dir)
            .expect("should derive path for standard dir");
        assert_eq!(derived, PathBuf::from("/data/claude-config.json"));
    }

    #[test]
    fn derive_mcp_path_from_override_supports_relative_rootless_dir() {
        let override_dir = PathBuf::from("claude");
        let derived = derive_mcp_path_from_override(&override_dir)
            .expect("should derive path for single segment");
        assert_eq!(derived, PathBuf::from("claude.json"));
    }

    #[test]
    fn derive_mcp_path_from_root_like_dir_returns_none() {
        let override_dir = PathBuf::from("/");
        assert!(derive_mcp_path_from_override(&override_dir).is_none());
    }

    #[test]
    fn sort_json_keys_sorts_top_level_object() {
        let input = serde_json::json!({
            "z": 1,
            "a": 2,
            "m": 3,
        });
        let sorted = sort_json_keys(&input);
        let serialized = serde_json::to_string(&sorted).unwrap();
        assert_eq!(serialized, r#"{"a":2,"m":3,"z":1}"#);
    }

    #[test]
    fn sort_json_keys_recurses_into_nested_objects() {
        let input = serde_json::json!({
            "outer_b": {"z": 1, "a": 2},
            "outer_a": {"y": 3, "b": 4},
        });
        let sorted = sort_json_keys(&input);
        let serialized = serde_json::to_string(&sorted).unwrap();
        assert_eq!(
            serialized,
            r#"{"outer_a":{"b":4,"y":3},"outer_b":{"a":2,"z":1}}"#
        );
    }

    #[test]
    fn sort_json_keys_preserves_array_order() {
        let input = serde_json::json!([3, 1, 2]);
        let sorted = sort_json_keys(&input);
        let serialized = serde_json::to_string(&sorted).unwrap();
        assert_eq!(serialized, "[3,1,2]");
    }

    #[test]
    fn sort_json_keys_sorts_objects_inside_arrays_but_keeps_array_order() {
        let input = serde_json::json!([
            {"z": 1, "a": 2},
            {"y": 3, "b": 4},
        ]);
        let sorted = sort_json_keys(&input);
        let serialized = serde_json::to_string(&sorted).unwrap();
        assert_eq!(serialized, r#"[{"a":2,"z":1},{"b":4,"y":3}]"#);
    }

    #[test]
    fn sort_json_keys_passes_through_primitives() {
        let cases = vec![
            serde_json::json!("hello"),
            serde_json::json!(42),
            serde_json::json!(3.14),
            serde_json::json!(true),
            serde_json::json!(null),
        ];
        for value in cases {
            let sorted = sort_json_keys(&value);
            assert_eq!(sorted, value);
        }
    }

    #[test]
    fn sort_json_keys_handles_empty_collections() {
        let empty_obj = serde_json::json!({});
        assert_eq!(
            serde_json::to_string(&sort_json_keys(&empty_obj)).unwrap(),
            "{}"
        );

        let empty_arr = serde_json::json!([]);
        assert_eq!(
            serde_json::to_string(&sort_json_keys(&empty_arr)).unwrap(),
            "[]"
        );
    }

    #[test]
    fn sort_json_keys_produces_identical_output_for_different_insertion_orders() {
        // 核心保证：同一逻辑配置无论键的插入顺序如何，写出的字节序列必须一致。
        let mut a = Map::new();
        a.insert("env".to_string(), serde_json::json!({"PATH": "/usr/bin"}));
        a.insert("model".to_string(), serde_json::json!("claude-sonnet-4-5"));
        a.insert("permissions".to_string(), serde_json::json!({"allow": []}));

        let mut b = Map::new();
        b.insert("permissions".to_string(), serde_json::json!({"allow": []}));
        b.insert("model".to_string(), serde_json::json!("claude-sonnet-4-5"));
        b.insert("env".to_string(), serde_json::json!({"PATH": "/usr/bin"}));

        let sorted_a = sort_json_keys(&Value::Object(a));
        let sorted_b = sort_json_keys(&Value::Object(b));

        assert_eq!(
            serde_json::to_string(&sorted_a).unwrap(),
            serde_json::to_string(&sorted_b).unwrap(),
        );
    }

    // ── atomic_write tests ────────────────────────────────────────────────────

    #[test]
    fn atomic_write_creates_and_updates_regular_file() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let path = dir.path().join("test.json");

        atomic_write(&path, b"first").expect("first write");
        assert_eq!(std::fs::read(&path).unwrap(), b"first");

        atomic_write(&path, b"second").expect("second write");
        assert_eq!(std::fs::read(&path).unwrap(), b"second");
    }

    /// Unix-only: verify that atomic_write writes through a POSIX symlink without
    /// replacing the symlink entry itself.
    #[cfg(unix)]
    #[test]
    fn atomic_write_preserves_posix_symlink() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let real = dir.path().join("real.json");
        let link = dir.path().join("link.json");

        std::fs::write(&real, b"original").expect("write real");
        std::os::unix::fs::symlink(&real, &link).expect("create symlink");

        atomic_write(&link, b"updated").expect("atomic_write through symlink");

        // The link entry must still be a symlink.
        assert!(
            link.symlink_metadata().unwrap().file_type().is_symlink(),
            "symlink was replaced by a regular file"
        );
        // The real file must contain the new content.
        assert_eq!(std::fs::read(&real).unwrap(), b"updated");
    }

    /// Windows-only: resolve_wsl_symlink must return None immediately for any
    /// path that is not a \\wsl.localhost\ or \\wsl$\ UNC path, without doing
    /// any IO or spawning WSL.
    #[cfg(windows)]
    #[test]
    fn resolve_wsl_symlink_ignores_non_wsl_paths() {
        for p in [
            r"C:\Users\foo\settings.json",
            r"D:\projects\bar.json",
            r"\\server\share\file.txt",
        ] {
            assert!(
                resolve_wsl_symlink(Path::new(p)).is_none(),
                "expected None for non-WSL path: {p}"
            );
        }
    }

    /// Windows + WSL integration test: atomic_write must write the new content
    /// to the real file and leave the WSL symlink entry intact.
    ///
    /// Skipped automatically when the default WSL distro is not running.
    #[cfg(windows)]
    #[test]
    fn atomic_write_preserves_wsl_symlink() {
        // Discover the default running WSL distro name.
        let distro = match wsl_default_distro() {
            Some(d) => d,
            None => {
                eprintln!("SKIP atomic_write_preserves_wsl_symlink: no WSL distro running");
                return;
            }
        };

        // 1. Create a real temp file on the Windows filesystem.
        let win_dir = tempfile::TempDir::new().expect("tempdir");
        let real_win = win_dir.path().join("settings.json");
        std::fs::write(&real_win, b"{}").expect("write real file");

        // 2. Translate the Windows path to a WSL /mnt/<drive>/... path.
        // We do this manually rather than spawning `wslpath -u` because wslpath
        // has trouble with backslash-separated paths when invoked via Command::new.
        let wsl_real = win_path_to_wsl(&real_win)
            .expect("could not convert temp dir path to WSL /mnt/ form");

        // 3. Create a WSL symlink in /tmp pointing at the real file.
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let wsl_link = format!("/tmp/cc_switch_test_{ts}.json");
        let ln_out = std::process::Command::new("wsl")
            .args(["-d", &distro, "--", "ln", "-sf", &wsl_real, &wsl_link])
            .output()
            .expect("ln -sf");
        assert!(ln_out.status.success(), "failed to create WSL test symlink");

        // 4. Build the Windows UNC path for the symlink.
        let wsl_link_win = wsl_link.replace('/', "\\").trim_start_matches('\\').to_owned();
        let unc = format!(r"\\wsl.localhost\{distro}\{wsl_link_win}");

        // 5. Write through the symlink.
        let new_content = br#"{"model":"claude-sonnet-4-7"}"#;
        let result = atomic_write(Path::new(&unc), new_content);

        // Always clean up the WSL symlink before asserting.
        let _ = std::process::Command::new("wsl")
            .args(["-d", &distro, "--", "rm", "-f", &wsl_link])
            .output();

        result.expect("atomic_write through WSL symlink should succeed");

        // 6. The real file must contain the new content.
        let written = std::fs::read(&real_win).expect("read real file after write");
        assert_eq!(
            written, new_content,
            "content was not written to the real file behind the WSL symlink"
        );

        // Symlink preservation is implied by step 6: if atomic_write had destroyed
        // the symlink and written to a new file at the UNC path instead, real_win
        // would still contain the original "{}" and the content assertion would fail.
    }

    /// Converts a Windows absolute path to the WSL `/mnt/<drive>/...` form.
    /// e.g. `C:\Users\foo\bar.json` → `/mnt/c/Users/foo/bar.json`
    #[cfg(windows)]
    fn win_path_to_wsl(path: &Path) -> Option<String> {
        let s = path.to_string_lossy();
        let mut chars = s.chars();
        let drive = chars.next()?.to_lowercase().next()?;
        if chars.next()? != ':' {
            return None;
        }
        let rest = chars.as_str().replace('\\', "/");
        Some(format!("/mnt/{drive}{rest}"))
    }

    /// Returns the name of the default running WSL distro, or None if WSL is
    /// unavailable or no distro is running.  Uses `$WSL_DISTRO_NAME` to avoid
    /// parsing UTF-16 output from `wsl -l`.
    #[cfg(windows)]
    fn wsl_default_distro() -> Option<String> {
        let out = std::process::Command::new("wsl")
            .args(["--", "bash", "-c", "echo $WSL_DISTRO_NAME"])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let name = String::from_utf8(out.stdout).ok()?;
        let name = name.trim().to_owned();
        if name.is_empty() {
            None
        } else {
            Some(name)
        }
    }
}

/// 复制文件
pub fn copy_file(from: &Path, to: &Path) -> Result<(), AppError> {
    fs::copy(from, to).map_err(|e| AppError::IoContext {
        context: format!("复制文件失败 ({} -> {})", from.display(), to.display()),
        source: e,
    })?;
    Ok(())
}

/// 删除文件
pub fn delete_file(path: &Path) -> Result<(), AppError> {
    if path.exists() {
        fs::remove_file(path).map_err(|e| AppError::io(path, e))?;
    }
    Ok(())
}

/// 检查 Claude Code 配置状态
#[derive(Serialize, Deserialize)]
pub struct ConfigStatus {
    pub exists: bool,
    pub path: String,
}

/// 获取 Claude Code 配置状态
pub fn get_claude_config_status() -> ConfigStatus {
    let path = get_claude_settings_path();
    ConfigStatus {
        exists: path.exists(),
        path: path.to_string_lossy().to_string(),
    }
}
