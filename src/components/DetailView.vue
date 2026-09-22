<script setup lang="ts">
import { computed, onMounted, ref, watch } from "vue";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import {
  api,
  formatBytes,
  type AppDetail,
  type DepotInfo,
  type ManifestVersion,
} from "../api";
import { appState, pushLog } from "../store";

const props = defineProps<{ appId: number }>();

const detail = ref<AppDetail | null>(null);
const loading = ref(true);
const loadError = ref("");

const selectedDepots = ref<Set<number>>(new Set());
const branch = ref("public");
const manifestId = ref("");
const installDir = ref("");
const fileFilters = ref("");
const verifyOnly = ref(false);
const starting = ref(false);
const historyDepot = ref<number | null>(null);
const historyList = ref<ManifestVersion[]>([]);
const historyLoading = ref(false);
const historyNote = ref("");

const settings = computed(() => appState.settings);
const isAnonymous = computed(
  () => appState.login.phase === "Online" && appState.login.anonymous
);

const filteredDepots = computed(() => {
  if (!detail.value) return [];
  return detail.value.depots.filter((d) => d.manifests.length > 0 || d.owner_app !== detail.value!.app_id);
});

const selectedSize = computed(() => {
  if (!detail.value) return 0;
  let total = 0;
  for (const d of detail.value.depots) {
    if (!selectedDepots.value.has(d.depot_id)) continue;
    const m = pickManifest(d);
    if (m) total += m.size;
  }
  return total;
});

function pickManifest(d: DepotInfo) {
  const mid = parseInt(manifestId.value, 10);
  if (mid > 0) return d.manifests.find((m) => m.branch === branch.value) ?? d.manifests[0];
  return d.manifests.find((m) => m.branch === branch.value) ?? d.manifests[0];
}

function toggleDepot(id: number) {
  if (selectedDepots.value.has(id)) selectedDepots.value.delete(id);
  else selectedDepots.value.add(id);
  selectedDepots.value = new Set(selectedDepots.value);
}

function autoSelectDepots() {
  if (!detail.value || !settings.value) return;
  const s = settings.value;
  const sel = new Set<number>();
  for (const d of detail.value.depots) {
    if (d.shared_install || d.low_violence) continue;
    if (d.os_list && !d.os_list.split(",").map((x) => x.trim()).includes(s.os)) continue;
    if (d.os_arch && s.arch && d.os_arch !== s.arch) continue;
    if (!s.all_languages && d.language && d.language !== s.language) continue;
    if (d.manifests.length === 0) continue;
    sel.add(d.depot_id);
  }
  selectedDepots.value = sel;
}

async function load() {
  loading.value = true;
  loadError.value = "";
  detail.value = null;
  try {
    detail.value = await api.getAppDetail(props.appId);
    autoSelectDepots();
    // lazy access check
    const ids = detail.value.depots.map((d) => d.depot_id);
    if (ids.length > 0) {
      api.checkDepotAccess(props.appId, ids).then((pairs) => {
        if (!detail.value) return;
        for (const [id, ok] of pairs) {
          const d = detail.value.depots.find((x) => x.depot_id === id);
          if (d) d.accessible = ok;
        }
      }).catch(() => {});
    }
  } catch (e) {
    loadError.value = String(e);
  } finally {
    loading.value = false;
  }
}

async function pickDir() {
  const dir = await openDialog({
    directory: true,
    defaultPath: installDir.value || settings.value?.install_dir || undefined,
  });
  if (typeof dir === "string") installDir.value = dir;
}

async function loadHistory(depotId: number) {
  if (historyDepot.value === depotId) {
    historyDepot.value = null;
    return;
  }
  historyDepot.value = depotId;
  historyList.value = [];
  historyLoading.value = true;
  historyNote.value = "";
  try {
    historyList.value = await api.getManifestHistory(props.appId, depotId);
    if (historyList.value.length === 0) {
      historyNote.value = "暂无历史记录（SteamDB 不可达时仅显示当前分支版本，可手动粘贴 Manifest GID）";
    } else if (!historyList.value.some((v) => v.source === "steamdb")) {
      historyNote.value = "SteamDB 历史不可达，仅显示当前各分支版本。可手动粘贴历史 Manifest GID。";
    }
  } catch (e) {
    historyNote.value = `获取历史失败: ${e}`;
  } finally {
    historyLoading.value = false;
  }
}

function useManifest(gid: number, branchName?: string | null) {
  manifestId.value = String(gid);
  if (branchName) branch.value = branchName;
  pushLog(`已选择 manifest ${gid}${branchName ? ` (${branchName})` : ""}`);
}

async function startDownload() {
  if (!installDir.value) {
    loadError.value = "请先选择安装目录";
    return;
  }
  starting.value = true;
  loadError.value = "";
  try {
    const mid = parseInt(manifestId.value, 10);
    const s = settings.value!;
    const id = await api.startDownload({
      app_id: props.appId,
      depot_ids: [...selectedDepots.value],
      branch: branch.value || "public",
      manifest_id: mid > 0 ? mid : null,
      install_dir: installDir.value,
      os: s.os,
      arch: s.arch,
      language: s.language,
      all_languages: s.all_languages,
      file_filters: fileFilters.value
        .split(/[;\n]/)
        .map((x) => x.trim())
        .filter(Boolean),
      verify_only: verifyOnly.value,
      max_connections: s.max_connections,
    });
    pushLog(`已创建下载任务 ${id.slice(0, 6)}（${detail.value?.name}）`);
    appState.view = "tasks";
  } catch (e) {
    loadError.value = String(e);
  } finally {
    starting.value = false;
  }
}

onMounted(() => {
  if (!installDir.value && settings.value?.install_dir) {
    installDir.value = settings.value.install_dir;
  }
  if (settings.value?.file_filters) fileFilters.value = settings.value.file_filters;
  load();
});

watch(() => props.appId, load);
</script>

<template>
  <div class="detail-view">
    <div v-if="loading" class="empty muted"><span class="spin"></span> 正在获取游戏信息...</div>
    <div v-else-if="loadError && !detail" class="empty">
      <p class="bright" style="margin-bottom: 8px">无法加载 App {{ appId }}</p>
      <p class="muted small">{{ loadError }}</p>
      <p class="muted small" style="margin-top: 8px">匿名登录只能访问免费游戏；付费游戏请先登录拥有该游戏的账号。</p>
      <button style="margin-top: 14px" @click="load">重试</button>
    </div>

    <template v-else-if="detail">
      <div class="header card">
        <img :src="detail.header_image" @error="($event.target as HTMLImageElement).style.display = 'none'" />
        <div class="header-info">
          <h2 class="bright">{{ detail.name }}</h2>
          <div class="muted small">
            AppID: {{ detail.app_id }}
            <span v-if="detail.free_to_download" class="badge green" style="margin-left: 8px">免费</span>
            <span v-for="os in detail.os_list" :key="os" class="badge" style="margin-left: 6px">{{ os }}</span>
          </div>
        </div>
        <button class="ghost" @click="appState.view = 'search'">← 返回搜索</button>
      </div>

      <div class="columns">
        <div class="col grow">
          <div class="card section">
            <h3>Depot 选择（{{ selectedDepots.size }} 个，约 {{ formatBytes(selectedSize) }}）</h3>
            <p class="muted small" style="margin: 6px 0">
              已按系统/架构/语言自动勾选；也可手动调整。
            </p>
            <div v-if="detail.depots.length === 0" class="muted small" style="padding: 12px 4px; line-height: 1.7">
              <template v-if="detail.depots_restricted && isAnonymous">
                Steam 对匿名用户隐藏了该游戏的 Depot 信息（免费游戏也需要用账号领取许可）。<br />
                请退出匿名模式，登录任意 Steam 账号后即可查看并下载。
              </template>
              <template v-else-if="detail.depots_restricted">
                当前账号无权访问该游戏的 Depot 信息（未拥有该游戏，且免费许可领取失败）。
              </template>
              <template v-else>该游戏没有可下载的 Depot。</template>
            </div>
            <table class="depot-table">
              <thead>
                <tr>
                  <th></th>
                  <th>Depot</th>
                  <th>系统</th>
                  <th>架构</th>
                  <th>语言</th>
                  <th>大小</th>
                  <th>权限</th>
                  <th></th>
                </tr>
              </thead>
              <tbody>
                <tr v-for="d in filteredDepots" :key="d.depot_id" :class="{ dim: d.shared_install }">
                  <td>
                    <input
                      type="checkbox"
                      :checked="selectedDepots.has(d.depot_id)"
                      @change="toggleDepot(d.depot_id)"
                      style="width: auto"
                    />
                  </td>
                  <td class="mono">{{ d.depot_id }}</td>
                  <td>{{ d.os_list || "通用" }}</td>
                  <td>{{ d.os_arch || "-" }}</td>
                  <td>{{ d.language || "-" }}</td>
                  <td class="mono">{{ formatBytes(pickManifest(d)?.size ?? 0) }}</td>
                  <td>
                    <span v-if="d.accessible === true" class="badge green">可下载</span>
                    <span v-else-if="d.accessible === false" class="badge red">无权</span>
                    <span v-else class="badge">未知</span>
                  </td>
                  <td>
                    <button class="ghost small" @click="loadHistory(d.depot_id)">
                      {{ historyDepot === d.depot_id ? "收起" : "历史版本" }}
                    </button>
                  </td>
                </tr>
              </tbody>
            </table>

            <div v-if="historyDepot" class="history">
              <h4>Depot {{ historyDepot }} 历史版本</h4>
              <p v-if="historyLoading" class="muted small"><span class="spin"></span> 加载中...</p>
              <table v-else-if="historyList.length" class="depot-table">
                <thead>
                  <tr><th>Manifest GID</th><th>日期</th><th>分支</th><th>来源</th><th></th></tr>
                </thead>
                <tbody>
                  <tr v-for="v in historyList" :key="v.manifest_id">
                    <td class="mono">{{ v.manifest_id }}</td>
                    <td>{{ v.date || "当前" }}</td>
                    <td>{{ v.branch || "-" }}</td>
                    <td class="muted small">{{ v.source === "steamdb" ? "SteamDB" : "当前分支" }}</td>
                    <td>
                      <button class="ghost small" @click="useManifest(v.manifest_id, v.branch)">使用此版本</button>
                    </td>
                  </tr>
                </tbody>
              </table>
              <p v-if="historyNote" class="muted small" style="margin-top: 6px">{{ historyNote }}</p>
            </div>
          </div>

          <div class="card section">
            <h3>分支（{{ detail.branches.length }}）</h3>
            <table class="depot-table" style="margin-top: 8px">
              <thead>
                <tr><th>分支</th><th>BuildID</th><th>说明</th><th></th></tr>
              </thead>
              <tbody>
                <tr v-for="b in detail.branches" :key="b.name" :class="{ dim: b.name !== 'public' && !b.description }">
                  <td class="bright">{{ b.name }}</td>
                  <td class="mono">{{ b.build_id }}</td>
                  <td class="muted small ellipsis" style="max-width: 260px">{{ b.description || "-" }}</td>
                  <td>
                    <button class="ghost small" @click="branch = b.name; manifestId = ''">
                      {{ branch === b.name && !manifestId ? "✓ 当前" : "选择" }}
                    </button>
                  </td>
                </tr>
              </tbody>
            </table>
          </div>
        </div>

        <div class="col side">
          <div class="card section">
            <h3>下载配置</h3>
            <div class="form">
              <div>
                <label>分支</label>
                <select v-model="branch" @change="manifestId = ''">
                  <option v-for="b in detail.branches" :key="b.name" :value="b.name">{{ b.name }}</option>
                  <option v-if="!detail.branches.length" value="public">public</option>
                </select>
              </div>
              <div>
                <label>Manifest GID（留空 = 分支当前版本；填入 GID = 指定历史版本）</label>
                <input type="text" v-model="manifestId" placeholder="例如 6436761910375028561" class="mono" />
              </div>
              <div>
                <label>安装目录</label>
                <div class="flex gap8">
                  <input type="text" v-model="installDir" placeholder="选择目录..." class="grow" />
                  <button @click="pickDir">浏览</button>
                </div>
              </div>
              <div>
                <label>文件过滤（正则，多个用分号分隔；留空 = 全部文件）</label>
                <input type="text" v-model="fileFilters" placeholder="例如 .*\.exe;maps/.*" class="mono" />
              </div>
              <div class="center gap8">
                <input type="checkbox" id="verify" v-model="verifyOnly" style="width: auto" />
                <label for="verify" style="margin: 0">仅校验/修复已有文件，不下载新内容</label>
              </div>
              <button
                class="success"
                style="width: 100%; padding: 10px; font-size: 14px"
                :disabled="starting || selectedDepots.size === 0"
                @click="startDownload"
              >
                <span v-if="starting"><span class="spin"></span> 创建中...</span>
                <span v-else>{{ verifyOnly ? "开始校验" : `开始下载（${formatBytes(selectedSize)}）` }}</span>
              </button>
              <p v-if="loadError" class="small" style="color: var(--red)">{{ loadError }}</p>
            </div>
          </div>

          <div class="card section muted small tips">
            <h3 class="bright" style="margin-bottom: 6px">提示</h3>
            <p>· 历史版本下载需要服务器返回 manifest 请求码；部分老版本已被开发者封锁，无法下载。</p>
            <p>· 下载会跳过与目标版本内容一致的已有文件，可用于版本切换/修复。</p>
            <p>· 付费游戏需要登录拥有该游戏的 Steam 账号。</p>
          </div>
        </div>
      </div>
    </template>
  </div>
</template>

<style scoped>
.detail-view { padding: 18px; overflow-y: auto; height: 100%; }
.empty { text-align: center; margin-top: 120px; }
.header { display: flex; gap: 14px; padding: 12px; align-items: center; }
.header img { width: 230px; height: 107px; object-fit: cover; border-radius: 3px; background: #0e151c; }
.header-info { flex: 1; min-width: 0; }
.header-info h2 { margin-bottom: 6px; }
.columns { display: flex; gap: 14px; margin-top: 14px; align-items: flex-start; }
.col.side { width: 340px; flex-shrink: 0; }
.section { padding: 14px; margin-bottom: 14px; }
.depot-table { width: 100%; border-collapse: collapse; font-size: 12px; }
.depot-table th {
  text-align: left;
  color: var(--text-dim);
  font-weight: normal;
  padding: 5px 8px;
  border-bottom: 1px solid var(--border);
}
.depot-table td { padding: 5px 8px; border-bottom: 1px solid rgba(255, 255, 255, 0.04); }
.depot-table tr.dim td { opacity: 0.5; }
.form { display: flex; flex-direction: column; gap: 10px; margin-top: 10px; }
.history { margin-top: 10px; padding-top: 10px; border-top: 1px dashed var(--border); }
.history h4 { margin-bottom: 8px; }
.tips p { margin: 4px 0; line-height: 1.5; }
</style>
