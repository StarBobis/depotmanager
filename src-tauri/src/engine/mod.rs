//! Download engine: depot resolution, file validation, parallel chunk
//! downloads with resume support, and progress reporting.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context};
use dashmap::DashMap;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, watch, Semaphore};

use crate::steamkit::cdn::{self, CdnClient};
use crate::steamkit::connection::CmConnection;
use crate::steamkit::manifest::{ChunkData, DepotManifest, FileData};
use crate::steamkit::pics::{package_grants_access, ProductInfoService};

pub const DEFAULT_BRANCH: &str = "public";
pub const CONFIG_DIR: &str = ".depotmanager";
/// Package id granted to all anonymous sessions.
pub const ANONYMOUS_PACKAGE: u32 = 17906;

// ============================================================
// Public types
// ============================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadRequest {
    pub app_id: u32,
    /// Explicit depot selection; empty = auto (filtered by os/arch/language).
    pub depot_ids: Vec<u32>,
    pub branch: String,
    /// Optional explicit manifest gid (applies to every selected depot).
    pub manifest_id: Option<u64>,
    pub install_dir: PathBuf,
    pub os: String,          // windows | macos | linux
    pub arch: String,        // 64 | 32 | "" (any)
    pub language: String,    // e.g. "english", "schinese", "" = only language-neutral
    pub all_languages: bool,
    pub file_filters: Vec<String>, // regex over normalized file paths; empty = all
    pub verify_only: bool,
    pub max_connections: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskState {
    Queued,
    Preparing,
    Downloading,
    Paused,
    Verifying,
    Completed,
    Failed,
    Canceled,
}

impl TaskState {
    pub fn is_active(&self) -> bool {
        matches!(self, TaskState::Queued | TaskState::Preparing | TaskState::Downloading | TaskState::Verifying)
    }
    pub fn label(&self) -> &'static str {
        match self {
            TaskState::Queued => "排队中",
            TaskState::Preparing => "准备中",
            TaskState::Downloading => "下载中",
            TaskState::Paused => "已暂停",
            TaskState::Verifying => "校验中",
            TaskState::Completed => "已完成",
            TaskState::Failed => "失败",
            TaskState::Canceled => "已取消",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskProgress {
    pub total_bytes: u64,
    pub downloaded_bytes: u64,
    pub total_files: usize,
    pub done_files: usize,
    pub speed_bps: f64,
    pub current_file: String,
    pub current_depot: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskSnapshot {
    pub id: String,
    pub state: TaskState,
    pub app_id: u32,
    pub app_name: String,
    pub install_dir: PathBuf,
    pub branch: String,
    pub manifest_id: Option<u64>,
    pub depot_ids: Vec<u32>,
    pub progress: TaskProgress,
    pub error: Option<String>,
    pub created_at: u64,
}

#[derive(Debug, Clone)]
pub enum EngineEvent {
    TaskSnapshot(TaskSnapshot),
    Log { task_id: String, line: String },
}

/// Controls a running task (pause / resume / cancel).
pub struct TaskControl {
    pub paused: watch::Sender<bool>,
    pub canceled: AtomicBool,
}

struct TaskInner {
    id: String,
    state: Mutex<TaskState>,
    request: DownloadRequest,
    app_name: Mutex<String>,
    progress_total: AtomicU64,
    progress_done: AtomicU64,
    total_files: Mutex<usize>,
    done_files: AtomicU64,
    current_file: Mutex<String>,
    current_depot: Mutex<u32>,
    speed_bps: Mutex<f64>,
    error: Mutex<Option<String>>,
    created_at: u64,
    control: Arc<TaskControl>,
}

pub struct DownloadTask {
    inner: Arc<TaskInner>,
}

impl DownloadTask {
    pub fn snapshot(&self) -> TaskSnapshot {
        let i = &self.inner;
        TaskSnapshot {
            id: i.id.clone(),
            state: *i.state.lock(),
            app_id: i.request.app_id,
            app_name: i.app_name.lock().clone(),
            install_dir: i.request.install_dir.clone(),
            branch: i.request.branch.clone(),
            manifest_id: i.request.manifest_id,
            depot_ids: i.request.depot_ids.clone(),
            progress: TaskProgress {
                total_bytes: i.progress_total.load(Ordering::Relaxed),
                downloaded_bytes: i.progress_done.load(Ordering::Relaxed),
                total_files: *i.total_files.lock(),
                done_files: i.done_files.load(Ordering::Relaxed) as usize,
                speed_bps: *i.speed_bps.lock(),
                current_file: i.current_file.lock().clone(),
                current_depot: *i.current_depot.lock(),
            },
            error: i.error.lock().clone(),
            created_at: i.created_at,
        }
    }

    pub fn pause(&self) {
        let _ = self.inner.control.paused.send(true);
        *self.inner.state.lock() = TaskState::Paused;
    }

    pub fn resume(&self) {
        let _ = self.inner.control.paused.send(false);
        if *self.inner.state.lock() == TaskState::Paused {
            *self.inner.state.lock() = TaskState::Downloading;
        }
    }

    pub fn cancel(&self) {
        self.inner.control.canceled.store(true, Ordering::Relaxed);
        let _ = self.inner.control.paused.send(false); // wake up waiters
    }

    pub fn is_canceled(&self) -> bool {
        self.inner.control.canceled.load(Ordering::Relaxed)
    }

    pub fn state(&self) -> TaskState {
        *self.inner.state.lock()
    }

    pub fn id(&self) -> &str {
        &self.inner.id
    }
}

// ============================================================
// Engine
// ============================================================

pub struct Engine {
    conn: Arc<CmConnection>,
    pics: Arc<ProductInfoService>,
    cdn: Arc<CdnClient>,
    pub events: broadcast::Sender<EngineEvent>,
    tasks: DashMap<String, Arc<DownloadTask>>,
    depot_keys: DashMap<u32, Vec<u8>>,
    anon_package_checked: AtomicBool,
}

impl Engine {
    pub fn new(
        conn: Arc<CmConnection>,
        pics: Arc<ProductInfoService>,
        cdn: Arc<CdnClient>,
        events: broadcast::Sender<EngineEvent>,
    ) -> Arc<Self> {
        Arc::new(Engine {
            conn,
            pics,
            cdn,
            events,
            tasks: DashMap::new(),
            depot_keys: DashMap::new(),
            anon_package_checked: AtomicBool::new(false),
        })
    }

    pub fn tasks(&self) -> Vec<TaskSnapshot> {
        let mut v: Vec<_> = self.tasks.iter().map(|t| t.snapshot()).collect();
        v.sort_by_key(|t| t.created_at);
        v
    }

    pub fn task(&self, id: &str) -> Option<Arc<DownloadTask>> {
        self.tasks.get(id).map(|t| t.clone())
    }

    pub fn remove_task(&self, id: &str) {
        if let Some(t) = self.tasks.get(id) {
            if !t.state().is_active() {
                drop(t);
                self.tasks.remove(id);
            }
        }
    }

    /// Queues a new download task and spawns its worker.
    pub fn start_download(self: &Arc<Self>, request: DownloadRequest) -> String {
        let id = new_task_id();
        let (paused_tx, _) = watch::channel(false);
        let inner = Arc::new(TaskInner {
            id: id.clone(),
            state: Mutex::new(TaskState::Queued),
            app_name: Mutex::new(String::new()),
            progress_total: AtomicU64::new(0),
            progress_done: AtomicU64::new(0),
            total_files: Mutex::new(0),
            done_files: AtomicU64::new(0),
            current_file: Mutex::new(String::new()),
            current_depot: Mutex::new(0),
            speed_bps: Mutex::new(0.0),
            error: Mutex::new(None),
            created_at: now_unix(),
            control: Arc::new(TaskControl {
                paused: paused_tx,
                canceled: AtomicBool::new(false),
            }),
            request,
        });
        let task = Arc::new(DownloadTask { inner });
        self.tasks.insert(id.clone(), task.clone());
        self.emit_snapshot(&task);

        let engine = self.clone();
        let task_id = id.clone();
        tokio::spawn(async move {
            engine.run_task(task.clone()).await;
            let _ = &task_id;
        });
        id
    }

    fn emit_snapshot(&self, task: &DownloadTask) {
        let _ = self.events.send(EngineEvent::TaskSnapshot(task.snapshot()));
    }

    fn log(&self, task_id: &str, line: impl Into<String>) {
        let _ = self.events.send(EngineEvent::Log {
            task_id: task_id.to_string(),
            line: line.into(),
        });
    }

    // --------------------------------------------------------
    // task lifecycle
    // --------------------------------------------------------

    async fn run_task(self: &Arc<Self>, task: Arc<DownloadTask>) {
        let result = self.run_task_inner(&task).await;
        match result {
            Ok(()) => {
                if task.is_canceled() {
                    *task.inner.state.lock() = TaskState::Canceled;
                    self.log(&task.inner.id, "任务已取消".to_string());
                } else {
                    *task.inner.state.lock() = TaskState::Completed;
                    self.log(&task.inner.id, "任务完成".to_string());
                }
            }
            Err(e) => {
                if task.is_canceled() {
                    *task.inner.state.lock() = TaskState::Canceled;
                    self.log(&task.inner.id, "任务已取消".to_string());
                } else {
                    *task.inner.state.lock() = TaskState::Failed;
                    *task.inner.error.lock() = Some(format!("{:#}", e));
                    self.log(&task.inner.id, format!("任务失败: {:#}", e));
                }
            }
        }
        self.emit_snapshot(&task);
    }

    async fn run_task_inner(self: &Arc<Self>, task: &Arc<DownloadTask>) -> anyhow::Result<()> {
        let req = task.inner.request.clone();
        *task.inner.state.lock() = TaskState::Preparing;
        self.emit_snapshot(task);

        // 1. app info (claim a free license first when the account lacks one,
        // otherwise the depots section is withheld)
        let is_anonymous = self.conn.state().steamid >> 52 & 0xF == 10;
        let info = self
            .pics
            .app_info_licensed(req.app_id, !is_anonymous)
            .await?
            .ok_or_else(|| anyhow!("无法获取 App {} 信息", req.app_id))?;
        let app_name = info.name().unwrap_or_else(|| format!("App {}", req.app_id));
        *task.inner.app_name.lock() = app_name.clone();
        self.log(&task.inner.id, format!("应用: {} ({})", app_name, req.app_id));
        self.emit_snapshot(task);

        // 2. resolve depot plans
        let plans = self.resolve_depots(task, &info.kv).await?;
        if plans.is_empty() {
            bail!("没有可下载的 depot（检查 depot 选择 / 系统架构 / 语言过滤 / 账号权限）");
        }

        // 3. ensure CDN servers
        if self.cdn.server_count() == 0 {
            let cell = self.conn.state().cell_id;
            let n = self.cdn.update_servers(cell, req.app_id).await?;
            self.log(&task.inner.id, format!("CDN 服务器: {} 个", n));
        }

        // 4. per-depot: keys + manifest + files
        for plan in &plans {
            self.check_control(task).await?;
            self.download_depot(task, plan).await?;
        }

        // speed display cleanup
        *task.inner.speed_bps.lock() = 0.0;
        Ok(())
    }

    async fn check_control(&self, task: &Arc<DownloadTask>) -> anyhow::Result<()> {
        if task.is_canceled() {
            bail!("canceled");
        }
        Ok(())
    }

    // --------------------------------------------------------
    // depot resolution
    // --------------------------------------------------------

    async fn resolve_depots(
        &self,
        task: &Arc<DownloadTask>,
        app_kv: &crate::steamkit::kv::KeyValue,
    ) -> anyhow::Result<Vec<DepotPlan>> {
        let req = &task.inner.request;
        let mut plans = Vec::new();

        let depots = app_kv
            .get("depots")
            .ok_or_else(|| anyhow!("appinfo 中没有 depots 段"))?;

        // collect candidate depot ids
        let mut candidates: Vec<u32> = if req.depot_ids.is_empty() {
            depots
                .children
                .iter()
                .filter(|c| c.name.chars().all(|ch| ch.is_ascii_digit()))
                .filter_map(|c| c.name.parse().ok())
                .collect()
        } else {
            req.depot_ids.clone()
        };
        candidates.sort_unstable();
        candidates.dedup();

        // anonymous access check: fetch the anonymous package once
        let is_anonymous = {
            let s = self.conn.state();
            s.steamid >> 52 & 0xF == 10 // account type AnonUser
        };
        if is_anonymous && !self.anon_package_checked.swap(true, Ordering::Relaxed) {
            let _ = self.pics.package_info(&[ANONYMOUS_PACKAGE]).await;
        }

        for depot_id in candidates {
            self.check_control(task).await?;
            let depot_kv = match depots.get(&depot_id.to_string()) {
                Some(d) => d.clone(),
                None => {
                    self.log(&task.inner.id, format!("跳过 depot {}: 不在 appinfo 中", depot_id));
                    continue;
                }
            };

            // config filters
            let config = depot_kv.get("config");
            if !depot_matches_filters(config, &req.os, &req.arch, &req.language, req.all_languages, req.depot_ids.is_empty()) {
                continue;
            }

            // resolve owner app (depotfromapp)
            let owner_app = depot_kv
                .get("depotfromapp")
                .and_then(|v| v.as_u32())
                .unwrap_or(req.app_id);

            // resolve manifest gid from (owner) appinfo
            let mut gid = req.manifest_id;
            if gid.is_none() {
                let owner_kv = if owner_app == req.app_id {
                    None
                } else {
                    match self.pics.app_info(owner_app).await? {
                        Some(info) => Some(info.kv.clone()),
                        None => {
                            self.log(&task.inner.id, format!("跳过 depot {}: 无法获取所属应用 {} 信息", depot_id, owner_app));
                            continue;
                        }
                    }
                };
                let depots_kv = owner_kv
                    .as_ref()
                    .and_then(|kv| kv.get("depots"))
                    .map(|d| d.clone())
                    .unwrap_or_else(|| depots.clone());
                let depot_node = depots_kv.get(&depot_id.to_string()).cloned().unwrap_or(depot_kv.clone());
                gid = depot_node
                    .path(&format!("manifests/{}/gid", req.branch))
                    .and_then(|g| g.as_u64());
            }

            let gid = match gid {
                Some(g) if g != 0 => g,
                _ => {
                    self.log(&task.inner.id, format!("跳过 depot {}: 分支 '{}' 没有 manifest", depot_id, req.branch));
                    continue;
                }
            };

            // access check
            if !self
                .account_has_access(req.app_id, depot_id, is_anonymous)
                .await
            {
                self.log(&task.inner.id, format!("跳过 depot {}: 当前会话无权访问（需要拥有游戏或登录账号）", depot_id));
                continue;
            }

            plans.push(DepotPlan {
                depot_id,
                owner_app,
                manifest_gid: gid,
                branch: req.branch.clone(),
            });
        }

        Ok(plans)
    }

    /// Public access check used by the UI to show depot availability.
    pub async fn check_access(&self, app_id: u32, depot_id: u32) -> bool {
        let is_anonymous = {
            let s = self.conn.state();
            s.steamid >> 52 & 0xF == 10 // account type AnonUser
        };
        self.account_has_access(app_id, depot_id, is_anonymous).await
    }

    async fn account_has_access(&self, app_id: u32, depot_id: u32, is_anonymous: bool) -> bool {
        if is_anonymous {
            if let Some(pkg) = self
                .pics
                .package_info(&[ANONYMOUS_PACKAGE])
                .await
                .ok()
                .and_then(|mut m| m.remove(&ANONYMOUS_PACKAGE).flatten())
            {
                return package_grants_access(&pkg, app_id) || package_grants_access(&pkg, depot_id);
            }
            return false;
        }

        // account: check licensed packages
        let licenses = self.conn.state().licenses;
        let package_ids: Vec<u32> = licenses.iter().map(|(id, _)| *id).collect();
        if package_ids.is_empty() {
            return false;
        }
        if let Ok(map) = self.pics.package_info(&package_ids).await {
            for (_id, pkg) in map {
                if let Some(pkg) = pkg {
                    if package_grants_access(&pkg, app_id) || package_grants_access(&pkg, depot_id) {
                        return true;
                    }
                }
            }
        }

        // free-to-play fallback: request a free license
        if let Ok(Some(info)) = self.pics.app_info(app_id).await {
            if info.free_to_download() {
                if let Ok((granted_apps, granted_pkgs)) = self.pics.request_free_license(app_id).await {
                    if granted_apps.contains(&app_id) || !granted_pkgs.is_empty() {
                        return true;
                    }
                }
            }
        }
        false
    }

    async fn depot_key(&self, app_id: u32, depot_id: u32) -> anyhow::Result<Vec<u8>> {
        if let Some(k) = self.depot_keys.get(&depot_id) {
            return Ok(k.clone());
        }
        let key = cdn::get_depot_key(&self.conn, app_id, depot_id).await?;
        self.depot_keys.insert(depot_id, key.clone());
        Ok(key)
    }

    // --------------------------------------------------------
    // depot download
    // --------------------------------------------------------

    async fn download_depot(self: &Arc<Self>, task: &Arc<DownloadTask>, plan: &DepotPlan) -> anyhow::Result<()> {
        let req = task.inner.request.clone();
        *task.inner.current_depot.lock() = plan.depot_id;
        self.log(&task.inner.id, format!("Depot {} (manifest {})", plan.depot_id, plan.manifest_gid));

        let depot_key = self
            .depot_key(plan.owner_app, plan.depot_id)
            .await
            .with_context(|| format!("获取 depot {} 密钥失败", plan.depot_id))?;

        let code = self
            .cdn
            .get_manifest_request_code(plan.depot_id, plan.owner_app, plan.manifest_gid, &plan.branch)
            .await
            .unwrap_or(0);

        let manifest = self
            .cdn
            .download_manifest(plan.owner_app, plan.depot_id, plan.manifest_gid, code, &depot_key)
            .await
            .with_context(|| format!("下载 depot {} manifest 失败", plan.depot_id))?;

        // save manifest for resume/diff
        let config_dir = req.install_dir.join(CONFIG_DIR);
        let _ = std::fs::create_dir_all(&config_dir);
        let manifest_path = config_dir.join(format!("{}_{}.manifest", plan.depot_id, plan.manifest_gid));
        let manifest_bytes = manifest.serialize();
        if std::fs::write(&manifest_path, &manifest_bytes).is_ok() {
            use sha1::{Digest, Sha1};
            let _ = std::fs::write(
                config_dir.join(format!("{}_{}.manifest.sha", plan.depot_id, plan.manifest_gid)),
                Sha1::digest(&manifest_bytes),
            );
        }

        // old manifest (for diff)
        let old_manifest = self.load_previous_manifest(&config_dir, plan.depot_id, plan.manifest_gid);

        // file plan
        let filters = compile_filters(&req.file_filters);
        let mut files: Vec<&FileData> = manifest
            .files
            .iter()
            .filter(|f| !f.is_directory())
            .filter(|f| filters_match(&filters, &f.name))
            .collect();
        files.sort_by_key(|f| f.name.to_lowercase());

        // directories first
        for f in &manifest.files {
            if f.is_directory() && filters_match(&filters, &f.name) {
                let _ = std::fs::create_dir_all(safe_join(&req.install_dir, &f.name)?);
            }
        }

        let depot_total: u64 = files.iter().map(|f| f.size).sum();
        task.inner.progress_total.fetch_add(depot_total, Ordering::Relaxed);
        *task.inner.total_files.lock() += files.len();
        self.emit_snapshot(task);

        // build work items with validation
        let mut work: Vec<FileWork> = Vec::new();
        for file in files {
            self.check_control(task).await?;
            let rel = normalize_rel(&file.name);
            let path = safe_join(&req.install_dir, &file.name)?;

            // manifest-diff shortcut: identical content hash => skip entirely
            if !req.verify_only {
                if let Some(old) = &old_manifest {
                    if let Some(old_file) = old.files.iter().find(|o| o.name == file.name) {
                        if old_file.hash == file.hash && old_file.size == file.size && path.exists() {
                            task.inner.progress_done.fetch_add(file.size, Ordering::Relaxed);
                            task.inner.done_files.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                    }
                }
            }

            work.push(FileWork::new(file.clone(), rel, path));
        }

        // validation pass (adler32 per chunk)
        let mut any_work = false;
        for w in &mut work {
            self.check_control(task).await?;
            *task.inner.current_file.lock() = w.rel.clone();
            let needs = w.validate().with_context(|| format!("校验文件 {} 失败", w.rel))?;
            let existing_bytes = w.size - needs.iter().map(|c| c.uncompressed_len as u64).sum::<u64>();
            task.inner.progress_done.fetch_add(existing_bytes, Ordering::Relaxed);
            if needs.is_empty() {
                task.inner.done_files.fetch_add(1, Ordering::Relaxed);
            } else {
                any_work = true;
                w.needed = needs;
            }
        }

        if req.verify_only {
            let bad: Vec<&FileWork> = work.iter().filter(|w| !w.needed.is_empty()).collect();
            if bad.is_empty() {
                self.log(&task.inner.id, "校验完成: 所有文件完好");
            } else {
                self.log(&task.inner.id, format!("校验完成: {} 个文件需要修复", bad.len()));
            }
            return Ok(());
        }

        if !any_work {
            self.log(&task.inner.id, format!("Depot {} 所有文件已是最新", plan.depot_id));
            self.emit_snapshot(task);
            return Ok(());
        }

        // preallocate files
        for w in &work {
            if !w.needed.is_empty() {
                w.preallocate()?;
            }
        }

        // parallel chunk download
        *task.inner.state.lock() = TaskState::Downloading;
        self.emit_snapshot(task);
        let work = Arc::new(work);
        self.download_chunks(task, plan, &depot_key, work).await?;

        Ok(())
    }

    async fn download_chunks(
        self: &Arc<Self>,
        task: &Arc<DownloadTask>,
        plan: &DepotPlan,
        depot_key: &[u8],
        work: Arc<Vec<FileWork>>,
    ) -> anyhow::Result<()> {
        let req = &task.inner.request;

        // flatten chunk jobs
        struct ChunkJob {
            file_idx: usize,
            chunk: ChunkData,
        }
        let mut jobs: std::collections::VecDeque<ChunkJob> = Default::default();
        for (i, w) in work.iter().enumerate() {
            for c in &w.needed {
                jobs.push_back(ChunkJob { file_idx: i, chunk: c.clone() });
            }
        }
        let total_jobs = jobs.len();
        self.log(&task.inner.id, format!("开始下载 depot {}: {} 个分块", plan.depot_id, total_jobs));

        let jobs = Arc::new(Mutex::new(jobs));
        let semaphore = Arc::new(Semaphore::new(req.max_connections.max(1)));
        let first_error: Arc<Mutex<Option<anyhow::Error>>> = Arc::new(Mutex::new(None));

        // speed sampler
        let sampler_done = Arc::new(AtomicBool::new(false));
        {
            let task = task.clone();
            let sampler_done = sampler_done.clone();
            let engine = self.clone();
            tokio::spawn(async move {
                let mut last = 0u64;
                let mut last_t = Instant::now();
                while !sampler_done.load(Ordering::Relaxed) {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    let now = task.inner.progress_done.load(Ordering::Relaxed);
                    let dt = last_t.elapsed().as_secs_f64();
                    if dt > 0.0 {
                        *task.inner.speed_bps.lock() = (now - last) as f64 / dt;
                    }
                    last = now;
                    last_t = Instant::now();
                    engine.emit_snapshot(&task);
                }
            });
        }

        let mut handles = Vec::new();
        for _ in 0..req.max_connections.max(1) {
            let jobs = jobs.clone();
            let task = task.clone();
            let mut pause_rx = task.inner.control.paused.subscribe();
            let first_error = first_error.clone();
            let semaphore = semaphore.clone();
            let engine = self.clone();
            let depot_key = depot_key.to_vec();
            let plan = plan.clone();
            let work = work.clone();

            let handle = tokio::spawn(async move {
                loop {
                    // control handling
                    if task.is_canceled() {
                        return;
                    }
                    if first_error.lock().is_some() {
                        return;
                    }
                    // pause support
                    while *pause_rx.borrow() {
                        if task.is_canceled() {
                            return;
                        }
                        if pause_rx.changed().await.is_err() {
                            return;
                        }
                    }

                    let job = {
                        let mut q = jobs.lock();
                        q.pop_front()
                    };
                    let Some(job) = job else { return };

                    let _permit = semaphore.acquire().await.unwrap();
                    let w = &work[job.file_idx];
                    *task.inner.current_file.lock() = w.rel.clone();

                    match engine
                        .cdn
                        .download_chunk(plan.owner_app, plan.depot_id, &job.chunk, &depot_key)
                        .await
                    {
                        Ok(data) => {
                            if let Err(e) = w.write_chunk(&job.chunk, &data) {
                                let mut fe = first_error.lock();
                                if fe.is_none() {
                                    *fe = Some(e.context(format!("写入文件 {} 失败", w.rel)));
                                }
                                return;
                            }
                            task.inner
                                .progress_done
                                .fetch_add(job.chunk.uncompressed_len as u64, Ordering::Relaxed);
                            if w.mark_chunk_done() {
                                task.inner.done_files.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        Err(e) => {
                            let mut fe = first_error.lock();
                            if fe.is_none() {
                                *fe = Some(e);
                            }
                            return;
                        }
                    }
                }
            });
            handles.push(handle);
        }

        for h in handles {
            let _ = h.await;
        }
        sampler_done.store(true, Ordering::Relaxed);

        if task.is_canceled() {
            bail!("canceled");
        }
        if let Some(e) = first_error.lock().take() {
            return Err(e);
        }

        self.log(&task.inner.id, format!("Depot {} 下载完成", plan.depot_id));
        self.emit_snapshot(task);
        Ok(())
    }

    fn load_previous_manifest(&self, config_dir: &Path, depot_id: u32, _current_gid: u64) -> Option<DepotManifest> {
        // Any stored manifest for this depot works as a diff base (content
        // hashes decide what to skip; a same-gid manifest is the common case
        // when re-downloading the same version).
        let entries = std::fs::read_dir(config_dir).ok()?;
        let mut candidates: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with(&format!("{}_", depot_id)) && n.ends_with(".manifest"))
                    .unwrap_or(false)
            })
            .collect();
        candidates.sort();
        for path in candidates.into_iter().rev() {
            let data = std::fs::read(&path).ok()?;
            // verify sidecar sha1
            let sha_path = path.with_extension("manifest.sha");
            if let Ok(expected) = std::fs::read(&sha_path) {
                use sha1::{Digest, Sha1};
                if Sha1::digest(&data).as_slice() != expected.as_slice() {
                    continue;
                }
            }
            if let Ok(m) = DepotManifest::parse(&data) {
                return Some(m);
            }
        }
        None
    }
}

// ============================================================
// helpers
// ============================================================

#[derive(Clone)]
struct DepotPlan {
    depot_id: u32,
    owner_app: u32,
    manifest_gid: u64,
    branch: String,
}

/// A file with its chunk work tracked.
struct FileWork {
    rel: String,
    path: PathBuf,
    size: u64,
    chunks: Vec<ChunkData>,
    needed: Vec<ChunkData>,
    remaining: AtomicUsize,
    file: Mutex<Option<Arc<std::fs::File>>>,
}

impl FileWork {
    fn new(file: FileData, rel: String, path: PathBuf) -> Self {
        FileWork {
            rel,
            path,
            size: file.size,
            chunks: file.chunks,
            needed: Vec::new(),
            remaining: AtomicUsize::new(0),
            file: Mutex::new(None),
        }
    }

    /// Validates the on-disk file against chunk checksums; returns needed chunks.
    fn validate(&mut self) -> anyhow::Result<Vec<ChunkData>> {
        if self.size == 0 {
            // empty file: just make sure it exists
            if let Some(parent) = self.path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            if !self.path.exists() {
                std::fs::File::create(&self.path)?;
            }
            return Ok(Vec::new());
        }

        let meta = std::fs::metadata(&self.path);
        let needs_alloc = match &meta {
            Ok(m) => m.len() != self.size,
            Err(_) => true,
        };

        if needs_alloc {
            return Ok(self.chunks.clone());
        }

        // read through the file and adler32 each chunk region
        let mut f = std::fs::File::open(&self.path)?;
        let mut sorted: Vec<&ChunkData> = self.chunks.iter().collect();
        sorted.sort_by_key(|c| c.offset);
        let mut needed = Vec::new();
        let mut buf = vec![0u8; 64 * 1024];
        for chunk in sorted {
            use std::io::{Read, Seek, SeekFrom};
            f.seek(SeekFrom::Start(chunk.offset))?;
            let mut remaining = chunk.uncompressed_len as usize;
            let mut a: u32 = 0;
            let mut b: u32 = 0;
            while remaining > 0 {
                let take = remaining.min(buf.len());
                let n = f.read(&mut buf[..take])?;
                if n == 0 {
                    break;
                }
                for &byte in &buf[..n] {
                    a = (a + byte as u32) % 65521;
                    b = (b + a) % 65521;
                }
                remaining -= n;
            }
            let crc = (b << 16) | a;
            if crc != chunk.checksum {
                #[cfg(debug_assertions)]
                eprintln!(
                    "[validate] {} chunk {} @{} len {} crc {:08x} != {:08x}",
                    self.rel, chunk.id_hex(), chunk.offset, chunk.uncompressed_len, crc, chunk.checksum
                );
                needed.push(chunk.clone());
            }
        }
        Ok(needed)
    }

    /// Preallocates/resizes the file and opens a shared handle for writes.
    fn preallocate(&self) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&self.path)?;
        file.set_len(self.size)?;
        *self.file.lock() = Some(Arc::new(file));
        self.remaining.store(self.needed.len(), Ordering::Relaxed);
        Ok(())
    }

    fn write_chunk(&self, chunk: &ChunkData, data: &[u8]) -> anyhow::Result<()> {
        let guard = self.file.lock();
        let file = guard.as_ref().context("file not open")?;
        write_at(file, data, chunk.offset)?;
        Ok(())
    }

    fn mark_chunk_done(&self) -> bool {
        self.remaining.fetch_sub(1, Ordering::Relaxed) == 1
    }
}

#[cfg(windows)]
fn write_at(file: &std::fs::File, buf: &[u8], offset: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    file.seek_write(buf, offset).map(|_| ())
}

#[cfg(unix)]
fn write_at(file: &std::fs::File, buf: &[u8], offset: u64) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.write_at(buf, offset)
}

/// Applies os/arch/language filters to a depot config node.
fn depot_matches_filters(
    config: Option<&crate::steamkit::kv::KeyValue>,
    os: &str,
    arch: &str,
    language: &str,
    all_languages: bool,
    is_auto_selection: bool,
) -> bool {
    let Some(config) = config else { return true };

    if let Some(oslist) = config.get("oslist").and_then(|v| v.value_str()) {
        if !oslist.is_empty() && !os.is_empty() && !oslist.split(',').any(|o| o.trim() == os) {
            return false;
        }
    }
    if let Some(osarch) = config.get("osarch").and_then(|v| v.value_str()) {
        if !osarch.is_empty() && !arch.is_empty() && osarch != arch {
            return false;
        }
    }
    // low violence depots are excluded from auto selection
    if is_auto_selection {
        if config.get("lowviolence").map(|v| v.as_bool()).unwrap_or(false) {
            return false;
        }
        // shared install redistributables (directx etc.) are skipped by default
        if let Some(depot_kv) = config.get("sharedinstall") {
            if depot_kv.as_bool() {
                return false;
            }
        }
    }
    if !all_languages {
        if let Some(lang) = config.get("language").and_then(|v| v.value_str()) {
            if !lang.is_empty() && lang != language {
                return false;
            }
        }
    }
    true
}

fn compile_filters(filters: &[String]) -> Vec<regex::Regex> {
    filters
        .iter()
        .filter_map(|f| regex::Regex::new(&format!("(?i){}", f)).ok())
        .collect()
}

fn filters_match(filters: &[regex::Regex], name: &str) -> bool {
    if filters.is_empty() {
        return true;
    }
    let normalized = name.replace('\\', "/");
    filters.iter().any(|r| r.is_match(&normalized))
}

/// Normalizes a manifest filename into a safe relative path (forward slashes).
fn normalize_rel(name: &str) -> String {
    name.replace('\\', "/")
}

/// Joins a manifest-relative path onto the install dir, rejecting traversal.
fn safe_join(base: &Path, rel: &str) -> anyhow::Result<PathBuf> {
    let normalized = rel.replace('\\', "/");
    let mut out = base.to_path_buf();
    for part in normalized.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." || part.contains(':') {
            bail!("非法文件路径: {}", rel);
        }
        out.push(part);
    }
    Ok(out)
}

fn new_task_id() -> String {
    use rand::RngCore;
    let mut b = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut b);
    hex::encode(b)
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
