<script setup lang="ts">
import { computed, onUnmounted, ref, watch } from "vue";
import QRCode from "qrcode";
import { api, type AuthChallenge } from "../api";
import { appState, pushLog } from "../store";

const open = ref(false);
const tab = ref<"password" | "qr" | "saved">("password");
const account = ref("");
const password = ref("");
const remember = ref(true);
const guardCode = ref("");
const challenge = ref<AuthChallenge | null>(null);
const qrUrl = ref("");
const qrDataUrl = ref("");
const busy = ref(false);
const errorMsg = ref("");

let pollTimer: number | null = null;

const online = computed(() => appState.login.phase === "Online");
const savedAccounts = computed(() => appState.settings?.accounts ?? []);

function stopPolling() {
  if (pollTimer !== null) {
    clearInterval(pollTimer);
    pollTimer = null;
  }
}

async function startPolling() {
  stopPolling();
  pollTimer = window.setInterval(async () => {
    try {
      const state = await api.authPoll();
      if (state && state.phase === "Online") {
        stopPolling();
        challenge.value = null;
        qrUrl.value = "";
        busy.value = false;
        open.value = false;
        pushLog(`登录成功: ${state.phase === "Online" ? state.account_name : ""}`);
      } else if (state && state.phase === "Offline") {
        stopPolling();
        busy.value = false;
      }
    } catch (e) {
      stopPolling();
      busy.value = false;
      errorMsg.value = String(e);
    }
  }, 2000);
}

async function doAnonymous() {
  busy.value = true;
  errorMsg.value = "";
  try {
    await api.loginAnonymous();
    open.value = false;
  } catch (e) {
    errorMsg.value = String(e);
  } finally {
    busy.value = false;
  }
}

async function doSaved(name: string) {
  busy.value = true;
  errorMsg.value = "";
  try {
    await api.loginSaved(name);
    open.value = false;
  } catch (e) {
    errorMsg.value = String(e);
  } finally {
    busy.value = false;
  }
}

async function doPassword() {
  if (!account.value || !password.value) {
    errorMsg.value = "请输入账号和密码";
    return;
  }
  busy.value = true;
  errorMsg.value = "";
  challenge.value = null;
  try {
    challenge.value = await api.loginPassword(account.value, password.value, remember.value);
    startPolling();
  } catch (e) {
    errorMsg.value = String(e);
    busy.value = false;
  }
}

async function doQr() {
  busy.value = true;
  errorMsg.value = "";
  try {
    qrUrl.value = await api.loginQr();
    qrDataUrl.value = await QRCode.toDataURL(qrUrl.value, { width: 220, margin: 1 });
    startPolling();
  } catch (e) {
    errorMsg.value = String(e);
    busy.value = false;
  }
}

async function submitCode() {
  if (!guardCode.value) return;
  errorMsg.value = "";
  try {
    const codeType = challenge.value?.kind === "email_code" ? 2 : 3;
    await api.loginSubmitCode(guardCode.value.trim(), codeType);
    guardCode.value = "";
    challenge.value = { kind: "confirmation", message: "验证码已提交，等待确认..." };
  } catch (e) {
    errorMsg.value = String(e);
  }
}

async function doLogout() {
  stopPolling();
  await api.authCancel().catch(() => {});
  await api.logout().catch(() => {});
  open.value = false;
  challenge.value = null;
  qrUrl.value = "";
}

async function removeAccount(name: string) {
  await api.deleteSavedAccount(name).catch(() => {});
  if (appState.settings) {
    appState.settings.accounts = appState.settings.accounts.filter(
      (a) => a.account_name !== name
    );
  }
}

watch(open, (v) => {
  if (!v) {
    stopPolling();
    api.authCancel().catch(() => {});
  }
});

onUnmounted(stopPolling);
</script>

<template>
  <div class="login-widget">
    <button v-if="!online" class="primary" @click="open = !open">
      <span v-if="appState.login.phase === 'Connecting' || appState.login.phase === 'LoggingOn' || appState.login.phase === 'Authenticating'">
        <span class="spin"></span> 登录中...
      </span>
      <span v-else>登录 Steam</span>
    </button>
    <template v-else>
      <span class="badge green">
        {{ appState.login.phase === "Online" ? appState.login.account_name : "" }}
        <template v-if="appState.login.phase === 'Online' && appState.login.anonymous">（匿名）</template>
      </span>
      <button class="ghost" @click="doLogout">退出</button>
    </template>

    <div v-if="open && !online" class="login-pop card">
      <div class="tabs">
        <button class="ghost" :class="{ active: tab === 'password' }" @click="tab = 'password'">账号密码</button>
        <button class="ghost" :class="{ active: tab === 'qr' }" @click="tab = 'qr'; doQr()">扫码登录</button>
        <button class="ghost" :class="{ active: tab === 'saved' }" @click="tab = 'saved'">已存账号</button>
      </div>

      <div v-if="tab === 'password'" class="pane">
        <label>Steam 账号</label>
        <input type="text" v-model="account" placeholder="账号名称" @keyup.enter="doPassword" />
        <label style="margin-top: 8px">密码</label>
        <input type="password" v-model="password" placeholder="密码" @keyup.enter="doPassword" />
        <div class="center gap8" style="margin-top: 8px">
          <input type="checkbox" id="remember" v-model="remember" style="width: auto" />
          <label for="remember" style="margin: 0">记住此账号（保存登录令牌，不保存密码）</label>
        </div>
        <button class="primary" style="width: 100%; margin-top: 10px" :disabled="busy" @click="doPassword">
          <span v-if="busy && !challenge"><span class="spin"></span> 验证中...</span>
          <span v-else>登录</span>
        </button>

        <div v-if="challenge && challenge.kind !== 'none'" class="challenge">
          <p class="small">{{ challenge.message }}</p>
          <div v-if="challenge.kind === 'email_code' || challenge.kind === 'device_code'" class="flex gap8" style="margin-top: 6px">
            <input type="text" v-model="guardCode" placeholder="验证码" @keyup.enter="submitCode" />
            <button @click="submitCode">提交</button>
          </div>
          <p v-else class="small muted" style="margin-top: 6px"><span class="spin"></span> 等待确认...</p>
        </div>
      </div>

      <div v-else-if="tab === 'qr'" class="pane center" style="flex-direction: column">
        <img v-if="qrDataUrl" :src="qrDataUrl" class="qr" alt="QR" />
        <p v-else class="small muted"><span class="spin"></span> 正在获取二维码...</p>
        <p class="small muted" style="margin-top: 8px">使用 Steam 手机应用扫码登录</p>
        <button class="ghost small" style="margin-top: 4px" @click="doQr">刷新二维码</button>
      </div>

      <div v-else class="pane">
        <p v-if="savedAccounts.length === 0" class="small muted">暂无保存的账号</p>
        <div v-for="acc in savedAccounts" :key="acc.account_name" class="saved-row">
          <span class="grow ellipsis">{{ acc.account_name }}</span>
          <button class="small" @click="doSaved(acc.account_name)" :disabled="busy">登录</button>
          <button class="ghost small" @click="removeAccount(acc.account_name)">删除</button>
        </div>
      </div>

      <div class="pane" style="border-top: 1px solid var(--border)">
        <button class="ghost" style="width: 100%" @click="doAnonymous" :disabled="busy">
          匿名登录（仅能下载免费游戏）
        </button>
      </div>

      <p v-if="errorMsg" class="error small">{{ errorMsg }}</p>
    </div>
  </div>
</template>

<style scoped>
.login-widget { position: relative; }
.login-pop {
  position: absolute;
  right: 0;
  top: calc(100% + 8px);
  width: 340px;
  z-index: 100;
  box-shadow: 0 8px 30px rgba(0, 0, 0, 0.6);
}
.tabs { display: flex; border-bottom: 1px solid var(--border); }
.tabs .ghost { flex: 1; border-radius: 0; padding: 9px; color: var(--text-dim); }
.tabs .ghost.active { color: var(--text-bright); border-bottom: 2px solid var(--accent); }
.pane { padding: 14px; }
.qr { width: 220px; height: 220px; background: white; border-radius: 4px; }
.challenge {
  margin-top: 10px;
  padding: 10px;
  background: rgba(102, 192, 244, 0.08);
  border: 1px solid var(--accent-dark);
  border-radius: 3px;
}
.saved-row {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 6px 0;
  border-bottom: 1px solid rgba(255, 255, 255, 0.05);
}
.error { color: var(--red); padding: 0 14px 12px; word-break: break-all; }
</style>
