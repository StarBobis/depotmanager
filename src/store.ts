import { reactive } from "vue";
import { listen } from "@tauri-apps/api/event";
import { api, type LoginState, type Settings, type TaskSnapshot } from "./api";

export const appState = reactive({
  login: { phase: "Offline", message: "初始化..." } as LoginState,
  settings: null as Settings | null,
  tasks: new Map<string, TaskSnapshot>(),
  logs: [] as string[],
  // ui
  view: "search" as "search" | "detail" | "tasks" | "settings",
  selectedAppId: null as number | null,
  logVisible: false,
});

export function pushLog(line: string) {
  const time = new Date().toLocaleTimeString("zh-CN", { hour12: false });
  appState.logs.push(`[${time}] ${line}`);
  if (appState.logs.length > 500) appState.logs.splice(0, appState.logs.length - 500);
}

export async function initStore() {
  try {
    appState.login = await api.getLoginState();
  } catch {}
  try {
    appState.settings = await api.getSettings();
  } catch {}
  try {
    const tasks = await api.taskList();
    for (const t of tasks) appState.tasks.set(t.id, t);
  } catch {}

  await listen<LoginState>("steam://state", (e) => {
    appState.login = e.payload;
  });
  await listen<{ line: string }>("steam://log", (e) => {
    pushLog(e.payload.line);
  });
  await listen<TaskSnapshot>("task://snapshot", (e) => {
    appState.tasks.set(e.payload.id, e.payload);
  });
}
