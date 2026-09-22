<script setup lang="ts">
import { computed } from "vue";
import { openPath } from "@tauri-apps/plugin-opener";
import {
  api,
  formatBytes,
  formatEta,
  formatSpeed,
  taskStateLabel,
  type TaskSnapshot,
} from "../api";
import { appState } from "../store";

const tasks = computed(() =>
  [...appState.tasks.values()].sort((a, b) => b.created_at - a.created_at)
);

const activeCount = computed(
  () => tasks.value.filter((t) => ["Queued", "Preparing", "Downloading", "Verifying"].includes(t.state)).length
);

function percent(t: TaskSnapshot): number {
  if (t.progress.total_bytes === 0) return 0;
  return Math.min(100, (t.progress.downloaded_bytes / t.progress.total_bytes) * 100);
}

async function pause(t: TaskSnapshot) { await api.pauseTask(t.id).catch(() => {}); }
async function resume(t: TaskSnapshot) { await api.resumeTask(t.id).catch(() => {}); }
async function cancel(t: TaskSnapshot) { await api.cancelTask(t.id).catch(() => {}); }
async function remove(t: TaskSnapshot) {
  await api.removeTask(t.id).catch(() => {});
  appState.tasks.delete(t.id);
}
async function openDir(t: TaskSnapshot) { await openPath(t.install_dir).catch(() => {}); }
</script>

<template>
  <div class="tasks-view">
    <div class="flex center" style="padding: 18px 18px 0">
      <h2 class="bright grow">下载队列（{{ tasks.length }}<template v-if="activeCount">，{{ activeCount }} 进行中</template>）</h2>
    </div>

    <div v-if="tasks.length === 0" class="empty muted">
      暂无下载任务。搜索游戏并选择版本后开始下载。
    </div>

    <div class="task-list">
      <div v-for="t in tasks" :key="t.id" class="task card">
        <div class="flex center gap12">
          <div class="grow" style="min-width: 0">
            <div class="flex center gap8">
              <span class="bright ellipsis">{{ t.app_name || `App ${t.app_id}` }}</span>
              <span class="badge">{{ taskStateLabel(t.state) }}</span>
              <span class="muted small">分支 {{ t.branch }}</span>
              <span v-if="t.manifest_id" class="muted small mono">GID {{ t.manifest_id }}</span>
            </div>
            <div class="muted small ellipsis" style="margin-top: 3px">{{ t.install_dir }}</div>
          </div>

          <div class="actions">
            <button v-if="t.state === 'Downloading'" class="small" @click="pause(t)">暂停</button>
            <button v-if="t.state === 'Paused'" class="small primary" @click="resume(t)">继续</button>
            <button
              v-if="['Queued', 'Preparing', 'Downloading', 'Paused', 'Verifying'].includes(t.state)"
              class="small danger"
              @click="cancel(t)"
            >取消</button>
            <template v-else>
              <button class="small ghost" @click="openDir(t)">打开目录</button>
              <button class="small ghost" @click="remove(t)">移除</button>
            </template>
          </div>
        </div>

        <div class="progress-row">
          <div class="bar">
            <div
              class="bar-fill"
              :class="{ done: t.state === 'Completed', error: t.state === 'Failed' }"
              :style="{ width: percent(t) + '%' }"
            ></div>
          </div>
          <div class="stats small">
            <span class="mono">{{ percent(t).toFixed(1) }}%</span>
            <span class="mono">{{ formatBytes(t.progress.downloaded_bytes) }} / {{ formatBytes(t.progress.total_bytes) }}</span>
            <span v-if="t.state === 'Downloading'" class="mono" style="color: var(--green)">{{ formatSpeed(t.progress.speed_bps) }}</span>
            <span v-if="t.state === 'Downloading' && t.progress.speed_bps > 0" class="muted mono">
              剩余 {{ formatEta(t.progress.total_bytes, t.progress.downloaded_bytes, t.progress.speed_bps) }}
            </span>
            <span class="muted mono">文件 {{ t.progress.done_files }}/{{ t.progress.total_files }}</span>
          </div>
        </div>

        <div v-if="t.state === 'Downloading' && t.progress.current_file" class="muted small mono ellipsis" style="margin-top: 4px">
          {{ t.progress.current_file }}
        </div>
        <div v-if="t.error" class="small" style="color: var(--red); margin-top: 4px; word-break: break-all">
          {{ t.error }}
        </div>
      </div>
    </div>
  </div>
</template>

<style scoped>
.tasks-view { height: 100%; display: flex; flex-direction: column; }
.empty { text-align: center; margin-top: 120px; }
.task-list { padding: 14px 18px 18px; overflow-y: auto; display: flex; flex-direction: column; gap: 10px; }
.task { padding: 12px 14px; }
.actions { display: flex; gap: 6px; flex-shrink: 0; }
.progress-row { margin-top: 10px; }
.bar {
  height: 8px;
  background: var(--bg-input);
  border-radius: 4px;
  overflow: hidden;
}
.bar-fill {
  height: 100%;
  background: linear-gradient(90deg, #47a0d8, #66c0f4);
  transition: width 0.3s;
}
.bar-fill.done { background: linear-gradient(90deg, #5a7a2e, #a1cd44); }
.bar-fill.error { background: var(--red); }
.stats { display: flex; gap: 14px; margin-top: 5px; }
</style>
