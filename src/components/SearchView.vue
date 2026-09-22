<script setup lang="ts">
import { onMounted, ref } from "vue";
import { api, type SearchResult } from "../api";
import { appState } from "../store";

const query = ref("");
const results = ref<SearchResult[]>([]);
const popular = ref<SearchResult[]>([]);
const searching = ref(false);
const searched = ref(false);
const appIdInput = ref("");

let debounce: number | null = null;

function onInput() {
  if (debounce) clearTimeout(debounce);
  debounce = window.setTimeout(doSearch, 350);
}

async function doSearch() {
  const q = query.value.trim();
  if (!q) {
    results.value = [];
    searched.value = false;
    return;
  }
  searching.value = true;
  try {
    results.value = await api.searchGames(q);
    searched.value = true;
  } catch {
    results.value = [];
    searched.value = true;
  } finally {
    searching.value = false;
  }
}

function openApp(appId: number) {
  appState.selectedAppId = appId;
  appState.view = "detail";
}

function openById() {
  const id = parseInt(appIdInput.value.trim(), 10);
  if (id > 0) {
    appIdInput.value = "";
    openApp(id);
  }
}

onMounted(async () => {
  if (popular.value.length === 0) {
    popular.value = await api.popularGames().catch(() => []);
  }
});
</script>

<template>
  <div class="search-view">
    <div class="search-bar card">
      <input
        type="text"
        v-model="query"
        @input="onInput"
        placeholder="搜索游戏名称，例如 Counter-Strike、Dota、PUBG..."
        class="grow"
      />
      <button class="primary" @click="doSearch" :disabled="searching">
        <span v-if="searching" class="spin"></span>
        搜索
      </button>
    </div>

    <div class="appid-bar card">
      <span class="muted small">已知 AppID？</span>
      <input
        type="text"
        v-model="appIdInput"
        placeholder="输入 AppID 直接打开，例如 730"
        style="max-width: 260px"
        @keyup.enter="openById"
      />
      <button @click="openById">打开</button>
    </div>

    <div v-if="results.length > 0" class="results">
      <div
        v-for="r in results"
        :key="r.app_id"
        class="result card"
        @click="openApp(r.app_id)"
      >
        <img :src="r.image ?? ''" loading="lazy" @error="($event.target as HTMLImageElement).style.visibility = 'hidden'" />
        <div class="result-info">
          <div class="bright ellipsis">{{ r.name }}</div>
          <div class="muted small">AppID: {{ r.app_id }}</div>
        </div>
      </div>
    </div>
    <div v-else-if="searched && !searching" class="empty muted">
      没有找到匹配的游戏。可以尝试英文名称，或直接使用 AppID 打开。
    </div>
    <div v-else-if="!searched">
      <h3 class="muted" style="margin-top: 18px">🔥 热门游戏</h3>
      <div v-if="popular.length > 0" class="results">
        <div
          v-for="r in popular"
          :key="r.app_id"
          class="result card"
          @click="openApp(r.app_id)"
        >
          <img :src="r.image ?? ''" loading="lazy" @error="($event.target as HTMLImageElement).style.visibility = 'hidden'" />
          <div class="result-info">
            <div class="bright ellipsis">{{ r.name }}</div>
            <div class="muted small">AppID: {{ r.app_id }}</div>
          </div>
        </div>
      </div>
      <div v-else class="empty muted" style="margin-top: 60px">
        <p style="font-size: 16px; margin-bottom: 8px">🎮 DepotManager</p>
        <p>搜索并浏览 Steam 游戏，查看历史版本，下载指定版本到本地。</p>
        <p class="small" style="margin-top: 12px">匿名登录可下载免费游戏；登录账号可下载已拥有的游戏。</p>
      </div>
    </div>
  </div>
</template>

<style scoped>
.search-view { padding: 18px; overflow-y: auto; height: 100%; }
.search-bar { display: flex; gap: 10px; padding: 10px; }
.appid-bar {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 8px 10px;
  margin-top: 10px;
}
.results {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(300px, 1fr));
  gap: 10px;
  margin-top: 14px;
}
.result {
  display: flex;
  gap: 10px;
  padding: 8px;
  cursor: pointer;
  transition: border-color 0.15s, transform 0.1s;
}
.result:hover { border-color: var(--accent-dark); transform: translateY(-1px); }
.result img { width: 120px; height: 45px; object-fit: cover; border-radius: 2px; background: #0e151c; }
.result-info { min-width: 0; display: flex; flex-direction: column; justify-content: center; gap: 4px; }
.empty { text-align: center; margin-top: 120px; }
</style>
