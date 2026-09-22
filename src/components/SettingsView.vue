<script setup lang="ts">
import { ref } from "vue";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { api, type Settings } from "../api";
import { appState, pushLog } from "../store";

const draft = ref<Settings | null>(null);
const saved = ref(false);

function ensureDraft(): Settings {
  if (!draft.value) {
    draft.value = JSON.parse(JSON.stringify(appState.settings)) as Settings;
  }
  return draft.value;
}

async function pickInstallDir() {
  const d = ensureDraft();
  const dir = await openDialog({ directory: true, defaultPath: d.install_dir || undefined });
  if (typeof dir === "string") d.install_dir = dir;
}

async function save() {
  if (!draft.value) return;
  draft.value.max_connections = Math.min(64, Math.max(1, draft.value.max_connections || 16));
  await api.updateSettings(draft.value).catch(() => {});
  appState.settings = draft.value;
  draft.value = null;
  saved.value = true;
  pushLog("设置已保存");
  setTimeout(() => (saved.value = false), 2000);
}

const languages = [
  ["english", "English"],
  ["schinese", "简体中文"],
  ["tchinese", "繁體中文"],
  ["japanese", "日本語"],
  ["koreana", "한국어"],
  ["russian", "Русский"],
  ["german", "Deutsch"],
  ["french", "Français"],
  ["spanish", "Español"],
] as const;
</script>

<template>
  <div class="settings-view" v-if="appState.settings">
    <h2 class="bright" style="margin-bottom: 14px">设置</h2>

    <div class="card section">
      <h3>下载</h3>
      <div class="form">
        <div>
          <label>默认安装目录（新建下载时的初始目录）</label>
          <div class="flex gap8">
            <input type="text" class="grow" :value="ensureDraft().install_dir" @input="ensureDraft().install_dir = ($event.target as HTMLInputElement).value" placeholder="未设置" />
            <button @click="pickInstallDir">浏览</button>
          </div>
        </div>
        <div>
          <label>并行连接数（1-64，越高越快，视网络而定；建议 16）</label>
          <input type="number" style="max-width: 120px" :value="ensureDraft().max_connections" @input="ensureDraft().max_connections = parseInt(($event.target as HTMLInputElement).value) || 16" />
        </div>
      </div>
    </div>

    <div class="card section">
      <h3>Depot 自动选择</h3>
      <div class="form">
        <div>
          <label>操作系统</label>
          <select style="max-width: 200px" :value="ensureDraft().os" @change="ensureDraft().os = ($event.target as HTMLSelectElement).value">
            <option value="windows">Windows</option>
            <option value="macos">macOS</option>
            <option value="linux">Linux</option>
          </select>
        </div>
        <div>
          <label>架构</label>
          <select style="max-width: 200px" :value="ensureDraft().arch" @change="ensureDraft().arch = ($event.target as HTMLSelectElement).value">
            <option value="64">64 位</option>
            <option value="32">32 位</option>
            <option value="">不限</option>
          </select>
        </div>
        <div>
          <label>语言（只下载对应语言包）</label>
          <select style="max-width: 200px" :value="ensureDraft().language" @change="ensureDraft().language = ($event.target as HTMLSelectElement).value">
            <option v-for="[v, label] in languages" :key="v" :value="v">{{ label }}</option>
          </select>
        </div>
        <div class="center gap8">
          <input type="checkbox" id="alllang" style="width: auto" :checked="ensureDraft().all_languages" @change="ensureDraft().all_languages = ($event.target as HTMLInputElement).checked" />
          <label for="alllang" style="margin: 0">下载所有语言包</label>
        </div>
      </div>
    </div>

    <div class="card section">
      <h3>高级</h3>
      <div class="form">
        <div>
          <label>区域 Cell ID（0 = 自动）</label>
          <input type="number" style="max-width: 120px" :value="ensureDraft().cell_id" @input="ensureDraft().cell_id = parseInt(($event.target as HTMLInputElement).value) || 0" />
        </div>
      </div>
    </div>

    <div class="flex gap12">
      <button class="primary" @click="save" :disabled="!draft">保存设置</button>
      <span v-if="saved" class="badge green center">已保存</span>
      <span v-if="draft" class="muted small center">有未保存的修改</span>
    </div>
  </div>
</template>

<style scoped>
.settings-view { padding: 18px; overflow-y: auto; height: 100%; max-width: 720px; }
.section { padding: 14px; margin-bottom: 14px; }
.form { display: flex; flex-direction: column; gap: 12px; margin-top: 10px; }
</style>
