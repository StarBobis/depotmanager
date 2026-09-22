<script setup lang="ts">
import { computed, nextTick, onMounted, ref, watch } from "vue";
import { appState, initStore } from "./store";
import LoginPanel from "./components/LoginPanel.vue";
import SearchView from "./components/SearchView.vue";
import DetailView from "./components/DetailView.vue";
import TasksView from "./components/TasksView.vue";
import SettingsView from "./components/SettingsView.vue";

const logEl = ref<HTMLElement | null>(null);

const activeCount = computed(
  () =>
    [...appState.tasks.values()].filter((t) =>
      ["Queued", "Preparing", "Downloading", "Verifying"].includes(t.state)
    ).length
);

const statusText = computed(() => {
  const s = appState.login;
  switch (s.phase) {
    case "Offline":
      return s.message || "离线";
    case "Connecting":
      return "正在连接 Steam...";
    case "Authenticating":
      return s.detail || "验证中...";
    case "LoggingOn":
      return "登录中...";
    case "Online":
      return s.anonymous ? "匿名模式" : `已登录: ${s.account_name}`;
  }
});

watch(
  () => appState.logs.length,
  async () => {
    await nextTick();
    if (logEl.value) logEl.value.scrollTop = logEl.value.scrollHeight;
  }
);

onMounted(initStore);
</script>

<template>
  <div class="shell">
    <header class="topbar">
      <div class="brand" @click="appState.view = 'search'">
        <span class="logo">⬇</span>
        <span class="bright">DepotManager</span>
        <span class="muted small">Steam 历史版本下载器</span>
      </div>

      <nav class="nav">
        <button class="ghost" :class="{ active: appState.view === 'search' || appState.view === 'detail' }" @click="appState.view = 'search'">
          🔍 浏览游戏
        </button>
        <button class="ghost" :class="{ active: appState.view === 'tasks' }" @click="appState.view = 'tasks'">
          📥 下载<span v-if="activeCount" class="badge yellow" style="margin-left: 4px">{{ activeCount }}</span>
        </button>
        <button class="ghost" :class="{ active: appState.view === 'settings' }" @click="appState.view = 'settings'">
          ⚙ 设置
        </button>
      </nav>

      <div class="grow"></div>

      <span class="muted small status">{{ statusText }}</span>
      <button class="ghost small" @click="appState.logVisible = !appState.logVisible">
        {{ appState.logVisible ? "隐藏日志" : "日志" }}
      </button>
      <LoginPanel />
    </header>

    <main class="main">
      <SearchView v-if="appState.view === 'search'" />
      <DetailView v-else-if="appState.view === 'detail' && appState.selectedAppId" :app-id="appState.selectedAppId" />
      <TasksView v-else-if="appState.view === 'tasks'" />
      <SettingsView v-else-if="appState.view === 'settings'" />
    </main>

    <footer v-if="appState.logVisible" class="log-console" ref="logEl">
      <div v-for="(line, i) in appState.logs" :key="i" class="log-line mono small">{{ line }}</div>
      <div v-if="appState.logs.length === 0" class="muted small">暂无日志</div>
    </footer>
  </div>
</template>

<style scoped>
.shell { display: flex; flex-direction: column; height: 100%; }
.topbar {
  display: flex;
  align-items: center;
  gap: 18px;
  padding: 8px 16px;
  background: var(--bg);
  border-bottom: 1px solid var(--border);
  flex-shrink: 0;
}
.brand { display: flex; align-items: baseline; gap: 8px; cursor: pointer; }
.logo {
  display: inline-block;
  width: 22px; height: 22px;
  background: linear-gradient(180deg, #47a0d8, #2e6d96);
  border-radius: 4px;
  text-align: center;
  line-height: 22px;
  font-size: 13px;
  color: white;
  align-self: center;
}
.nav { display: flex; gap: 4px; }
.nav .ghost { padding: 6px 12px; color: var(--text-dim); }
.nav .ghost.active { color: var(--text-bright); background: rgba(102, 192, 244, 0.12); }
.status { max-width: 200px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.main { flex: 1; min-height: 0; }
.log-console {
  height: 160px;
  flex-shrink: 0;
  overflow-y: auto;
  background: #0d1217;
  border-top: 1px solid var(--border);
  padding: 8px 14px;
  user-select: text;
}
.log-line { padding: 1px 0; color: #9fb3c8; }
</style>
