import { invoke } from "@tauri-apps/api/core";

// ---------- types mirroring the Rust DTOs ----------

export type LoginState =
  | { phase: "Offline"; message: string }
  | { phase: "Connecting" }
  | { phase: "Authenticating"; detail: string }
  | { phase: "LoggingOn" }
  | { phase: "Online"; account_name: string; steamid: number; anonymous: boolean };

export interface AuthChallenge {
  kind: "none" | "email_code" | "device_code" | "confirmation" | "qr";
  message: string;
}

export interface SavedAccount {
  account_name: string;
  steamid: number;
  refresh_token: string;
  guard_data?: string | null;
}

export interface Settings {
  install_dir: string;
  max_connections: number;
  language: string;
  all_languages: boolean;
  os: string;
  arch: string;
  cell_id: number;
  accounts: SavedAccount[];
  last_account?: string | null;
  machine_id_seed: string;
  cm_endpoints: string[];
  file_filters: string;
}

export interface SearchResult {
  app_id: number;
  name: string;
  image?: string | null;
  source: string;
}

export interface BranchInfo {
  name: string;
  build_id: number;
  description?: string | null;
}

export interface DepotManifestRef {
  branch: string;
  manifest_id: number;
  size: number;
}

export interface DepotInfo {
  depot_id: number;
  owner_app: number;
  os_list: string;
  os_arch: string;
  language: string;
  shared_install: boolean;
  low_violence: boolean;
  manifests: DepotManifestRef[];
  accessible?: boolean | null;
}

export interface AppDetail {
  app_id: number;
  name: string;
  free_to_download: boolean;
  os_list: string[];
  branches: BranchInfo[];
  depots: DepotInfo[];
  header_image: string;
  depots_restricted: boolean;
}

export interface ManifestVersion {
  manifest_id: number;
  date?: string | null;
  source: string;
  branch?: string | null;
  build_id?: number | null;
  description?: string | null;
}

export type TaskStateName =
  | "Queued" | "Preparing" | "Downloading" | "Paused"
  | "Verifying" | "Completed" | "Failed" | "Canceled";

export interface TaskProgress {
  total_bytes: number;
  downloaded_bytes: number;
  total_files: number;
  done_files: number;
  speed_bps: number;
  current_file: string;
  current_depot: number;
}

export interface TaskSnapshot {
  id: string;
  state: TaskStateName;
  app_id: number;
  app_name: string;
  install_dir: string;
  branch: string;
  manifest_id?: number | null;
  depot_ids: number[];
  progress: TaskProgress;
  error?: string | null;
  created_at: number;
}

export interface StartDownloadArgs {
  app_id: number;
  depot_ids: number[];
  branch: string;
  manifest_id?: number | null;
  install_dir: string;
  os: string;
  arch: string;
  language: string;
  all_languages: boolean;
  file_filters: string[];
  verify_only: boolean;
  max_connections: number;
}

// ---------- invoke wrappers ----------

export const api = {
  getLoginState: () => invoke<LoginState>("get_login_state"),
  loginAnonymous: () => invoke<LoginState>("login_anonymous"),
  loginSaved: (accountName: string) => invoke<LoginState>("login_saved", { accountName }),
  loginPassword: (accountName: string, password: string, remember: boolean) =>
    invoke<AuthChallenge>("login_password", { accountName, password, remember }),
  loginQr: () => invoke<string>("login_qr"),
  loginSubmitCode: (code: string, codeType: number) =>
    invoke<void>("login_submit_code", { code, codeType }),
  authPoll: () => invoke<LoginState | null>("auth_poll"),
  authCancel: () => invoke<void>("auth_cancel"),
  logout: () => invoke<void>("logout"),

  searchGames: (query: string) => invoke<SearchResult[]>("search_games", { query }),
  popularGames: () => invoke<SearchResult[]>("popular_games"),
  getAppDetail: (appId: number) => invoke<AppDetail>("get_app_detail", { appId }),
  checkDepotAccess: (appId: number, depotIds: number[]) =>
    invoke<[number, boolean][]>("check_depot_access", { appId, depotIds }),
  getManifestHistory: (appId: number, depotId: number) =>
    invoke<ManifestVersion[]>("get_manifest_history", { appId, depotId }),
  appNameHint: (appId: number) => invoke<string | null>("app_name_hint", { appId }),

  startDownload: (args: StartDownloadArgs) => invoke<string>("start_download", { args }),
  taskList: () => invoke<TaskSnapshot[]>("task_list"),
  pauseTask: (taskId: string) => invoke<void>("pause_task", { taskId }),
  resumeTask: (taskId: string) => invoke<void>("resume_task", { taskId }),
  cancelTask: (taskId: string) => invoke<void>("cancel_task", { taskId }),
  removeTask: (taskId: string) => invoke<void>("remove_task", { taskId }),

  getSettings: () => invoke<Settings>("get_settings"),
  updateSettings: (settings: Settings) => invoke<void>("update_settings", { settings }),
  deleteSavedAccount: (accountName: string) =>
    invoke<void>("delete_saved_account", { accountName }),
};

// ---------- formatting helpers ----------

export function formatBytes(bytes: number): string {
  if (!bytes || bytes <= 0) return "0 B";
  const units = ["B", "KB", "MB", "GB", "TB"];
  const i = Math.min(units.length - 1, Math.floor(Math.log2(bytes) / 10));
  const v = bytes / Math.pow(1024, i);
  return `${v >= 100 ? v.toFixed(0) : v >= 10 ? v.toFixed(1) : v.toFixed(2)} ${units[i]}`;
}

export function formatSpeed(bps: number): string {
  return `${formatBytes(bps)}/s`;
}

export function formatEta(total: number, done: number, speed: number): string {
  if (speed <= 0 || total <= done) return "";
  const secs = Math.round((total - done) / speed);
  if (secs < 60) return `${secs} 秒`;
  if (secs < 3600) return `${Math.floor(secs / 60)} 分 ${secs % 60} 秒`;
  return `${Math.floor(secs / 3600)} 小时 ${Math.floor((secs % 3600) / 60)} 分`;
}

export function taskStateLabel(s: TaskStateName): string {
  return {
    Queued: "排队中",
    Preparing: "准备中",
    Downloading: "下载中",
    Paused: "已暂停",
    Verifying: "校验中",
    Completed: "已完成",
    Failed: "失败",
    Canceled: "已取消",
  }[s];
}
