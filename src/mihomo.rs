//! An outbound-only Mihomo sidecar. Imported documents never become configs.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::codex::routes::Route;
use crate::config::{AppPaths, Config, IpCheckMode, RouteConfig};

static EMBEDDED_MIHOMO: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/mihomo-core.bin"));

pub fn embedded_hash() -> String {
    crate::util::sha256_hex(EMBEDDED_MIHOMO)
}

fn materialize_embedded(root: &Path) -> Result<PathBuf> {
    if EMBEDDED_MIHOMO.is_empty() {
        bail!("当前构建没有内置 Mihomo；请使用 Release 安装器或关闭协议节点");
    }
    let runtime = root.join("runtime");
    std::fs::create_dir_all(&runtime)?;
    private_directory(&runtime)?;
    let expected = embedded_hash();
    let versioned = runtime.join(&expected);
    std::fs::create_dir_all(&versioned)?;
    private_directory(&versioned)?;
    let path = versioned.join("mihomo.exe");
    let valid_existing = path.is_file()
        && std::fs::read(&path)
            .ok()
            .is_some_and(|bytes| crate::util::sha256_hex(&bytes) == expected);
    if !valid_existing {
        let _ = std::fs::remove_file(&path);
        let temp = runtime.join(format!("{}.tmp", uuid::Uuid::new_v4()));
        std::fs::write(&temp, EMBEDDED_MIHOMO)?;
        std::fs::rename(&temp, &path)?;
    }
    Ok(path)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MihomoConfig {
    pub enabled: bool,
    /// Absolute path, or mihomo.exe beside the GPTGuard executable.
    pub executable: Option<String>,
    pub base_port: u16,
    pub startup_timeout_seconds: u64,
}

impl Default for MihomoConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            executable: None,
            base_port: 17890,
            startup_timeout_seconds: 10,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SidecarStatus {
    pub enabled: bool,
    pub ready: bool,
    pub pid: Option<u32>,
    pub restarts: u32,
    pub detail: String,
}

pub fn validate_node(node: &Value) -> Result<()> {
    let map = node.as_object().context("Mihomo node must be an object")?;
    let kind = node.get("type").and_then(Value::as_str).unwrap_or_default();
    if !matches!(
        kind,
        "ss" | "vmess" | "vless" | "trojan" | "hysteria" | "hysteria2" | "tuic" | "http" | "socks5"
    ) {
        bail!("unsupported Mihomo node type; use ss/vmess/vless/trojan/hysteria/hysteria2/tuic/http/socks5");
    }
    let server = node
        .get("server")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if server.is_empty() || server.chars().any(char::is_whitespace) {
        bail!("Mihomo node requires a server");
    }
    if !matches!(node.get("port").and_then(Value::as_u64), Some(1..=65535)) {
        bail!("Mihomo node port must be 1-65535");
    }
    for field in [
        "dialer-proxy",
        "proxy",
        "listeners",
        "proxy-providers",
        "rule-providers",
        "tun",
        "dns",
    ] {
        if map.contains_key(field) {
            bail!("node contains unsupported routing or listener fields");
        }
    }
    Ok(())
}

pub fn validate_config(config: &Config) -> Result<()> {
    let cfg = &config.mihomo;
    if !(1..=60).contains(&cfg.startup_timeout_seconds) {
        bail!("mihomo startup timeout must be 1-60 seconds");
    }
    if let Some(path) = &cfg.executable {
        if !Path::new(path).is_absolute() {
            bail!("mihomo.executable must be an absolute path");
        }
    }
    if !cfg.enabled {
        return Ok(());
    }
    if config.codex.direct {
        bail!("Mihomo mode requires codex.direct = false");
    }
    if config.protection.ip_check_mode != IpCheckMode::Http {
        bail!("Mihomo mode requires ip-check-mode http; STUN tests the physical connection");
    }
    let count = config.codex.routes.iter().filter(|r| r.enabled).count();
    if count == 0 || count > 128 {
        bail!("Mihomo mode requires 1-128 enabled routes");
    }
    let end = u32::from(cfg.base_port) + count as u32 - 1;
    if cfg.base_port < 1024 || end > 65535 {
        bail!("Mihomo listener port range is invalid");
    }
    let gateway: SocketAddr = config.codex.listen.parse()?;
    for port in [gateway.port(), config.ui.web_port] {
        if (u32::from(cfg.base_port)..=end).contains(&u32::from(port)) {
            bail!("Mihomo ports overlap the gateway or management panel");
        }
    }
    Ok(())
}

fn outbound(route: &RouteConfig) -> Result<Value> {
    if let Some(node) = &route.mihomo {
        validate_node(node)?;
        return Ok(node.clone());
    }
    crate::config::validate_proxy_url(&route.url)?;
    let url = url::Url::parse(&route.url)?;
    // URL userinfo is percent-encoded; use the same decoding as query values.
    let decode = |s: &str| -> String {
        url::form_urlencoded::parse(format!("x={}", s.replace('+', "%2B")).as_bytes())
            .next()
            .map(|(_, v)| v.into_owned())
            .unwrap_or_default()
    };
    let mut node = json!({
        "type": if url.scheme().starts_with("socks") { "socks5" } else { "http" },
        "server": url.host_str().unwrap().trim_matches(['[', ']']),
        "port": url.port().unwrap(),
    });
    if url.scheme() == "https" {
        node["tls"] = json!(true);
    }
    if !url.username().is_empty() {
        node["username"] = json!(decode(url.username()));
    }
    if let Some(password) = url.password() {
        node["password"] = json!(decode(password));
    }
    Ok(node)
}

pub struct Plan {
    pub config: Value,
    pub routes: Vec<RouteConfig>,
    ports: Vec<u16>,
    password: String,
}

/// One fixed listener per node: state route indices must never follow a
/// mutable GLOBAL/select group. MATCH,REJECT is a final fail-closed backstop.
pub fn generate(config: &Config) -> Result<Plan> {
    validate_config(config)?;
    let password = crate::config::random_token();
    let mut proxies = Vec::new();
    let mut listeners = Vec::new();
    let mut routes = Vec::new();
    let mut ports = Vec::new();
    for (index, route) in config.codex.routes.iter().filter(|r| r.enabled).enumerate() {
        let port = config
            .mihomo
            .base_port
            .checked_add(index as u16)
            .context("Mihomo port overflow")?;
        let name = format!("gptguard-{index}");
        let mut node = outbound(route)?;
        node["name"] = json!(name);
        proxies.push(node);
        listeners.push(json!({
            "name": format!("in-{index}"), "type": "mixed", "listen": "127.0.0.1",
            "port": port, "udp": false, "proxy": name,
            "users": [{"username": "gptguard", "password": password}],
        }));
        ports.push(port);
        routes.push(RouteConfig {
            id: route.id.clone(),
            label: route.label.clone(),
            enabled: true,
            mihomo: None,
            url: format!("socks5h://gptguard:{password}@127.0.0.1:{port}"),
        });
    }
    Ok(Plan {
        config: json!({
            "allow-lan": false, "bind-address": "127.0.0.1", "mode": "rule",
            "port": 0, "socks-port": 0, "mixed-port": 0, "redir-port": 0, "tproxy-port": 0,
            "external-controller": "", "external-controller-unix": "", "external-controller-pipe": "",
            "tun": {"enable": false}, "dns": {"enable": false},
            "profile": {"store-selected": false, "store-fake-ip": false},
            "find-process-mode": "off", "geo-auto-update": false, "log-level": "silent",
            "proxies": proxies, "listeners": listeners, "rules": ["MATCH,REJECT"],
        }),
        routes,
        ports,
        password,
    })
}

pub fn fingerprint(config: &Config) -> String {
    json!({"mihomo": config.mihomo, "routes": config.codex.routes, "direct": config.codex.direct})
        .to_string()
}

struct Worker {
    stop: Arc<AtomicBool>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Worker {
    fn shutdown(&self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.thread.lock().unwrap().take() {
            let _ = handle.join();
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[derive(Clone)]
pub struct Runtime {
    status: Arc<RwLock<SidecarStatus>>,
    _worker: Option<Arc<Worker>>,
}

impl Runtime {
    pub fn start(config: &Config, paths: &AppPaths) -> Result<(Self, Vec<Route>)> {
        let status = Arc::new(RwLock::new(SidecarStatus {
            enabled: config.mihomo.enabled,
            detail: "Mihomo 未启用".into(),
            ..Default::default()
        }));
        if !config.mihomo.enabled {
            return Ok((
                Self {
                    status,
                    _worker: None,
                },
                crate::codex::routes::build_routes(config)?,
            ));
        }
        let plan = generate(config)?;
        let routes = plan
            .routes
            .iter()
            .map(Route::from_config)
            .collect::<Result<Vec<_>>>()?;
        status.write().unwrap().detail = "Mihomo 启动中".into();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let worker_status = status.clone();
        let cfg = config.mihomo.clone();
        // Keep the released core at %LOCALAPPDATA%\GPTGuard\runtime\ and
        // ephemeral Mihomo configs in a separate private subdirectory.
        let root = paths.data_dir.clone();
        let handle = thread::Builder::new()
            .name("mihomo-supervisor".into())
            .spawn(move || {
                supervise(cfg, root, plan, &worker_stop, &worker_status);
            })?;
        Ok((
            Self {
                status,
                _worker: Some(Arc::new(Worker {
                    stop,
                    thread: Mutex::new(Some(handle)),
                })),
            },
            routes,
        ))
    }

    pub fn status(&self) -> SidecarStatus {
        self.status.read().unwrap().clone()
    }
    pub fn available(&self) -> bool {
        let s = self.status();
        !s.enabled || s.ready
    }

    pub fn shutdown(&self) {
        if let Some(worker) = &self._worker {
            worker.shutdown();
            let mut status = self.status.write().unwrap();
            status.ready = false;
            status.pid = None;
            status.detail = "Mihomo 已停止".into();
        }
    }
}

fn supervise(
    cfg: MihomoConfig,
    root: PathBuf,
    plan: Plan,
    stop: &AtomicBool,
    status: &RwLock<SidecarStatus>,
) {
    let mut attempt = 0u32;
    while !stop.load(Ordering::Acquire) {
        status.write().unwrap().restarts = attempt;
        let result = run_once(&cfg, &root, &plan, stop, status);
        let mut state = status.write().unwrap();
        state.ready = false;
        state.pid = None;
        state.detail = match result {
            Ok(()) => "Mihomo 已停止".into(),
            Err(err) => format!("Mihomo 不可用：{err}；网关已停止转发"),
        };
        state.restarts = attempt;
        drop(state);
        if stop.load(Ordering::Acquire) {
            break;
        }
        // Bounded backoff, interruptible by drop/shutdown.
        let delay = Duration::from_secs((1u64 << attempt.min(5)).min(30));
        pause(stop, delay);
        attempt = attempt.saturating_add(1);
    }
}

fn pause(stop: &AtomicBool, delay: Duration) {
    let deadline = Instant::now() + delay;
    while !stop.load(Ordering::Acquire) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
}

fn command(executable: &Path) -> Command {
    let mut cmd = Command::new(executable);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Do not let inherited proxy settings alter sidecar behaviour.
    for key in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ] {
        cmd.env_remove(key);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    cmd
}

struct ManagedChild {
    child: Child,
    #[cfg(windows)]
    _job: Job,
}

impl ManagedChild {
    fn spawn(cmd: &mut Command) -> Result<Self> {
        #[cfg(windows)]
        let job = Job::new()?;
        let mut child = cmd
            .spawn()
            .context("无法启动 Mihomo，请检查可执行文件路径")?;
        #[cfg(windows)]
        if let Err(err) = job.assign(&child) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(err);
        }
        Ok(Self {
            child,
            #[cfg(windows)]
            _job: job,
        })
    }
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct RunFiles(PathBuf);
impl Drop for RunFiles {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.0.join("config.yaml"));
        let _ = std::fs::remove_file(self.0.join("cache.db"));
        let _ = std::fs::remove_dir(&self.0);
    }
}

fn run_once(
    cfg: &MihomoConfig,
    root: &Path,
    plan: &Plan,
    stop: &AtomicBool,
    status: &RwLock<SidecarStatus>,
) -> Result<()> {
    let executable = match &cfg.executable {
        // Keep the explicit path only as a hidden recovery option for old
        // installations.  The normal release path always uses the embedded
        // core and never asks the user to download a second executable.
        Some(path) => PathBuf::from(path),
        None => materialize_embedded(root)?,
    };
    if !executable.is_file() {
        bail!("找不到 Mihomo 可执行文件");
    }
    let runs = root.join("mihomo");
    std::fs::create_dir_all(&runs)?;
    private_directory(&runs)?;
    let dir = runs.join(format!("run-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&dir)?;
    let files = RunFiles(dir.canonicalize()?);
    private_directory(&files.0)?;
    let config_file = files.0.join("config.yaml");
    std::fs::write(&config_file, serde_yaml::to_string(&plan.config)?)?;

    // Hold all ports while validating; occupied ports must never be mistaken
    // for our child during readiness checks.
    let reservations = plan
        .ports
        .iter()
        .map(|port| TcpListener::bind(("127.0.0.1", *port)))
        .collect::<std::io::Result<Vec<_>>>()
        .context("本地 Mihomo 端口已被占用")?;
    let mut test = ManagedChild::spawn(
        command(&executable)
            .arg("-t")
            .arg("-d")
            .arg(&files.0)
            .arg("-f")
            .arg(&config_file),
    )?;
    let deadline = Instant::now() + Duration::from_secs(cfg.startup_timeout_seconds);
    loop {
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }
        if let Some(exit) = test.child.try_wait()? {
            if !exit.success() {
                bail!("Mihomo 配置校验失败；请检查节点参数");
            }
            break;
        }
        if Instant::now() >= deadline {
            bail!("Mihomo 配置校验超时");
        }
        pause(stop, Duration::from_millis(50));
    }
    drop(test);
    drop(reservations);
    let mut process = ManagedChild::spawn(
        command(&executable)
            .arg("-d")
            .arg(&files.0)
            .arg("-f")
            .arg(&config_file),
    )?;
    status.write().unwrap().pid = Some(process.child.id());
    let deadline = Instant::now() + Duration::from_secs(cfg.startup_timeout_seconds);
    loop {
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }
        if process.child.try_wait()?.is_some() {
            bail!("Mihomo 进程退出");
        }
        if plan
            .ports
            .iter()
            .all(|port| authenticated_listener(*port, &plan.password))
        {
            break;
        }
        if Instant::now() >= deadline {
            bail!("Mihomo 监听端口就绪超时");
        }
        pause(stop, Duration::from_millis(100));
    }
    {
        let mut state = status.write().unwrap();
        state.ready = true;
        state.detail = format!("Mihomo 运行中，{} 个本地出口", plan.ports.len());
    }
    while !stop.load(Ordering::Acquire) {
        if process.child.try_wait()?.is_some() {
            bail!("Mihomo 进程退出");
        }
        pause(stop, Duration::from_millis(100));
    }
    Ok(())
}

fn authenticated_listener(port: u16, password: &str) -> bool {
    let check = || -> std::io::Result<bool> {
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(100))?;
        stream.set_read_timeout(Some(Duration::from_millis(100)))?;
        stream.set_write_timeout(Some(Duration::from_millis(100)))?;
        stream.write_all(&[5, 1, 2])?;
        let mut reply = [0; 2];
        stream.read_exact(&mut reply)?;
        if reply != [5, 2] {
            return Ok(false);
        }
        let mut auth = vec![1, 8];
        auth.extend_from_slice(b"gptguard");
        auth.push(password.len() as u8);
        auth.extend_from_slice(password.as_bytes());
        stream.write_all(&auth)?;
        stream.read_exact(&mut reply)?;
        Ok(reply == [1, 0])
    };
    check().unwrap_or(false)
}

fn private_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(windows)]
    {
        let output = command(Path::new("whoami.exe"))
            .args(["/user", "/fo", "csv", "/nh"])
            .stdout(Stdio::piped())
            .output()?;
        let text = String::from_utf8_lossy(&output.stdout);
        let sid = text
            .trim()
            .rsplit(',')
            .next()
            .unwrap_or_default()
            .trim_matches('"');
        if !sid.starts_with("S-1-")
            || !sid
                .bytes()
                .all(|c| c.is_ascii_digit() || c == b'S' || c == b'-')
        {
            bail!("无法确定 sidecar 目录的当前用户 SID");
        }
        let result = command(Path::new("icacls.exe"))
            .arg(path)
            .args(["/inheritance:r", "/grant:r", &format!("*{sid}:(OI)(CI)F")])
            .status()?;
        if !result.success() {
            bail!("无法保护 sidecar 配置目录");
        }
    }
    Ok(())
}

#[cfg(windows)]
struct Job(windows_sys::Win32::Foundation::HANDLE);
#[cfg(windows)]
impl Job {
    fn new() -> Result<Self> {
        use windows_sys::Win32::System::JobObjects::*;
        unsafe {
            let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if handle.is_null() {
                return Err(std::io::Error::last_os_error().into());
            }
            let job = Self(handle);
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                std::mem::size_of_val(&info) as u32,
            ) == 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            Ok(job)
        }
    }
    fn assign(&self, child: &Child) -> Result<()> {
        use std::os::windows::io::AsRawHandle;
        if unsafe {
            windows_sys::Win32::System::JobObjects::AssignProcessToJobObject(
                self.0,
                child.as_raw_handle(),
            )
        } == 0
        {
            bail!("无法将 Mihomo 加入 Windows Job Object");
        }
        Ok(())
    }
}
#[cfg(windows)]
impl Drop for Job {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        let mut cfg = Config::default();
        cfg.mihomo.enabled = true;
        cfg.codex.direct = false;
        cfg.protection.ip_check_mode = IpCheckMode::Http;
        cfg.codex.routes = vec![
            RouteConfig {
                id: "a".into(),
                label: "node".into(),
                url: String::new(),
                enabled: true,
                mihomo: Some(
                    json!({"type": "trojan", "server": "example.test", "port": 443, "password": "SECRET"}),
                ),
            },
            RouteConfig {
                id: "b".into(),
                label: "url".into(),
                url: "https://u:p%40ss@proxy.test:8443".into(),
                enabled: true,
                mihomo: None,
            },
        ];
        cfg
    }

    #[test]
    fn generated_config_has_only_fixed_loopback_listeners() {
        let plan = generate(&config()).unwrap();
        assert_eq!(plan.config["allow-lan"], false);
        assert_eq!(plan.config["tun"]["enable"], false);
        assert_eq!(plan.config["dns"]["enable"], false);
        assert_eq!(plan.config["external-controller"], "");
        assert!(plan.config.get("proxy-providers").is_none());
        assert!(plan.config.get("proxy-groups").is_none());
        assert_eq!(plan.config["rules"], json!(["MATCH,REJECT"]));
        for (index, listener) in plan.config["listeners"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
        {
            assert_eq!(listener["listen"], "127.0.0.1");
            assert_eq!(listener["udp"], false);
            assert_eq!(listener["proxy"], plan.config["proxies"][index]["name"]);
            assert_eq!(listener["port"], 17890 + index);
            assert_eq!(listener["users"][0]["password"], plan.password);
            assert!(plan.routes[index].url.contains("127.0.0.1"));
        }
        assert_eq!(plan.config["proxies"][1]["tls"], true);
        assert_eq!(plan.config["proxies"][1]["password"], "p@ss");
        assert_ne!(plan.password, generate(&config()).unwrap().password);
    }

    #[test]
    fn embedded_core_identity_is_stable() {
        assert_eq!(embedded_hash().len(), 64);
        assert_eq!(embedded_hash(), crate::util::sha256_hex(EMBEDDED_MIHOMO));
    }

    #[test]
    fn rejects_direct_empty_pool_and_conflicting_ports() {
        let mut cfg = config();
        cfg.codex.direct = true;
        assert!(cfg.validate().is_err());
        cfg.codex.direct = false;
        cfg.mihomo.base_port = 17841;
        assert!(cfg.validate().is_err());
        cfg.mihomo.base_port = 65535;
        assert!(cfg.validate().is_err());
        cfg.mihomo.base_port = 17890;
        cfg.codex.routes.clear();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn old_configuration_defaults_to_disabled_sidecar() {
        let cfg: Config = serde_json::from_value(json!({"version":1})).unwrap();
        cfg.validate().unwrap();
        assert!(!cfg.mihomo.enabled);
    }
}
