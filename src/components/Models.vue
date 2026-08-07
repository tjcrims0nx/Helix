<template>
  <div class="gate-overlay model-manager-overlay">
    <div class="manager-panel">
      <!-- Header (sticky) -->
      <div class="manager-header">
        <div class="manager-title-row">
          <div class="manager-glyph">
            <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="#fff" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M4 22h14a2 2 0 0 0 2-2V7.5L14.5 2H6a2 2 0 0 0-2 2v4"></path><polyline points="14 2 14 8 20 8"></polyline><path d="M2 15h10"></path><path d="M9 18l3-3-3-3"></path></svg>
          </div>
          <div>
            <h2 class="manager-title">MODEL MANAGER</h2>
            <p class="manager-subtitle">Manage downloaded models &amp; remote pulls</p>
          </div>
          <button class="manager-close" @click="$emit('close')">✕</button>
        </div>

        <!-- Search bar -->
        <div class="manager-search-row">
          <input
            v-model="localManagerSearch"
            class="manager-search"
            type="text"
            placeholder="Search models or HuggingFace (e.g. Llama-3, Qwen, Mistral, DeepSeek)…"
            spellcheck="false"
            @keydown.enter="localManagerTab === 'huggingface' && doHfSearch(localManagerSearch)"
          />
        </div>

        <!-- Tabs -->
        <div class="manager-tabs" style="grid-template-columns: repeat(2, minmax(120px, 1fr));">
          <button
            class="manager-tab"
            :class="{ active: localManagerTab === 'huggingface' || localManagerTab === 'ollama' }"
            @click="localManagerTab = 'huggingface'"
          >
            HUGGINGFACE
          </button>
          <button
            class="manager-tab"
            :class="{ active: localManagerTab === 'files' }"
            @click="localManagerTab = 'files'"
          >
            LOCAL MODELS
            <span class="manager-tab-count">{{ localGgufs.length }}</span>
          </button>
        </div>

        <!-- Active Download Banner -->
        <div v-if="pulling" class="active-download-banner">
          <div class="banner-top">
            <div class="banner-title-group">
              <span class="banner-pulse"></span>
              <span class="banner-label">DOWNLOADING MODEL</span>
              <code class="banner-model-name" :title="pulling.name">{{ pulling.name }}</code>
            </div>
            <div class="banner-stats">
              <span v-if="pulling.pct !== undefined" class="banner-pct">{{ pulling.pct.toFixed(0) }}%</span>
              <span v-if="pulling.bps" class="banner-rate">{{ fmtRate(pulling.bps) }}</span>
              <span v-if="pulling.eta && pulling.eta > 0" class="banner-eta">ETA {{ fmtEta(pulling.eta) }}</span>
            </div>
          </div>
          <div class="banner-progress-track">
            <div
              class="banner-progress-fill"
              :class="{ indeterminate: !pulling.pct }"
              :style="{ width: (pulling.pct || 0) + '%' }"
            ></div>
          </div>
          <div class="banner-subtext">
            <span>{{ pulling.status || 'Downloading chunks...' }}</span>
            <span v-if="pulling.completed && pulling.total">
              {{ fmtSizeGb(pulling.completed) }} / {{ fmtSizeGb(pulling.total) }}
            </span>
          </div>
        </div>
      </div>

      <!-- Scrollable Body -->
      <div class="manager-body">
        <!-- Raw Files Tab -->
        <template v-if="localManagerTab === 'files'">
          <div class="manager-disk-bar">
            <span class="manager-disk-label">RAW GGUF FILES</span>
            <span class="manager-disk-value">{{ formatBytes(totalGgufSize) }}</span>
          </div>

          <p class="manager-hint">
            Local GGUF models stored in <code>~/.HELIX/models/</code>. Executed directly by the built-in llama engine.
          </p>

          <div v-if="filteredGgufs.length === 0" class="manager-empty">
            <template v-if="localManagerSearch">No files matching "{{ localManagerSearch }}"</template>
            <template v-else>No local .gguf models found yet. Pull a model from HuggingFace or import a .gguf file.</template>
          </div>

          <div v-else class="manager-list">
            <div v-for="f in filteredGgufs" :key="f.name" class="model-card">
              <div class="model-card-main">
                <div class="model-card-icon file-icon">GG</div>
                <div class="model-card-info">
                  <div class="model-card-name" :title="f.name">{{ f.name }}</div>
                  <div class="model-card-meta">
                    <span class="model-tag size-tag">💾 {{ formatBytes(f.size) }}</span>
                  </div>
                </div>
              </div>
              <div class="model-card-actions">
                <button
                  class="model-action-btn use"
                  @click="$emit('update:selectedModel', f.name); $emit('close')"
                  title="Use this local GGUF model"
                >USE MODEL</button>
                <button
                  class="model-action-btn"
                  @click="$emit('moveGguf', f.name)"
                  :disabled="isDeletingGguf"
                  title="Move to another location"
                >MOVE</button>
                <button
                  class="model-action-btn danger"
                  @click="$emit('deleteGguf', f.name)"
                  :disabled="isDeletingGguf"
                  title="Permanently delete this file from your hard drive"
                >TRASH FILE</button>
              </div>
            </div>
          </div>
        </template>

        <!-- HuggingFace Tab -->
        <template v-if="localManagerTab === 'huggingface'">
          <div class="hf-search-bar">
            <input
              v-model="hfSearchQuery"
              class="manager-search"
              type="text"
              placeholder="Search HuggingFace GGUF models (e.g. Qwen, Llama-3, Mistral, DeepSeek)..."
              spellcheck="false"
              @keydown.enter="doHfSearch()"
            />
            <button class="import-btn hf-search-btn" @click="doHfSearch()" :disabled="hfSearching">
              {{ hfSearching ? 'SEARCHING…' : 'SEARCH' }}
            </button>
          </div>

          <p class="manager-hint">
            Browse open-source GGUF models directly from HuggingFace. Click a model to reveal download options.
          </p>

          <div v-if="hfSearching" class="manager-empty">
            Searching HuggingFace…
          </div>

          <div v-else-if="hfSearchResults.length === 0" class="manager-empty">
            No HuggingFace models found yet. Click Search to query models.
          </div>

          <div v-else class="manager-list">
            <div v-for="m in hfSearchResults" :key="m.repo_id" class="model-card hf-model-card">
              <div class="model-card-main hf-card-main" @click="toggleHfRepo(m.repo_id)">
                <div class="model-card-icon hf-icon">HF</div>
                <div class="model-card-info hf-card-info">
                  <div class="model-card-name" :title="m.repo_id">{{ m.repo_id }}</div>
                  <div class="model-card-meta hf-meta">
                    <span class="model-tag download-tag">⬇ {{ m.downloads.toLocaleString() }} downloads</span>
                    <span class="model-tag likes-tag">♥ {{ m.likes.toLocaleString() }} likes</span>
                    <span v-if="hfRepoFiles[m.repo_id]?.length" class="model-tag files-tag">💾 {{ hfRepoFiles[m.repo_id].length }} GGUF files</span>
                  </div>
                </div>
                <button class="model-action-btn use hf-toggle-btn">
                  {{ hfExpandedRepo === m.repo_id ? 'HIDE FILES ▲' : 'VIEW FILES ▼' }}
                </button>
              </div>

              <!-- Expanded Files -->
              <div v-if="hfExpandedRepo === m.repo_id" class="hf-files-container">
                <div v-if="hfLoadingFiles[m.repo_id]" class="manager-hint">Loading GGUF files from HuggingFace…</div>
                <div v-else-if="!hfRepoFiles[m.repo_id] || hfRepoFiles[m.repo_id].length === 0" class="manager-hint">No .gguf files found in this repository.</div>
                <div v-else class="quant-grid">
                  <div v-for="f in hfRepoFiles[m.repo_id]" :key="f.filename" class="quant-card">
                    <div class="quant-filename" :title="f.filename">
                      {{ f.filename }}
                    </div>
                    <div class="quant-details">
                      <span>Quant: <strong class="quant-label-hl">{{ f.quant_label }}</strong></span>
                      <span class="quant-size-pill">
                        💾 {{ f.size ? formatBytes(f.size) : 'Pending' }}
                      </span>
                    </div>
                    <button
                      class="model-action-btn use"
                      :disabled="pulling?.name.startsWith(f.filename.replace(/\.gguf$/i, ''))"
                      @click.stop="$emit('pullModel', m.repo_id, f.filename)"
                    >
                      <template v-if="pulling?.name.startsWith(f.filename.replace(/\.gguf$/i, ''))">PULLING…</template>
                      <template v-else>PULL {{ f.quant_label }}</template>
                    </button>
                  </div>
                </div>
              </div>
            </div>
          </div>
        </template>
      </div>

      <!-- Footer (pinned bottom) -->
      <div class="manager-footer">
        <button class="import-btn" @click="$emit('importGguf')" :disabled="isImporting">
          <span v-if="isImporting">IMPORTING…</span>
          <span v-else>⬆ IMPORT LOCAL GGUF</span>
        </button>
        <button class="close-modal-btn" @click="$emit('close')">DONE</button>
      </div>
    </div>
  </div>
</template>

<script setup lang="ts">
import { ref, watch, onMounted } from 'vue';
import { HuggingFaceModel, HuggingFaceFile } from '../types';

const props = defineProps<{
  managerSearch: string;
  managerTab: string;
  models: any[];
  localGgufs: any[];
  totalOllamaSize: number;
  filteredOllamaModels: any[];
  selectedModel: string;
  isDeletingModel: boolean;
  totalGgufSize: number;
  filteredGgufs: any[];
  activatedGgufs: Set<string>;
  isImporting: boolean;
  isDeletingGguf: boolean;
  searchHuggingFace: (query: string) => Promise<HuggingFaceModel[]>;
  listHuggingFaceFiles: (repoId: string) => Promise<HuggingFaceFile[]>;
  pulling: any;
  vramGb: string | null;
  localStatus: any;
  formatBytes: (bytes: number) => string;
  modelKey: (name: string) => string;
  quantFit: (size: number) => string;
  fmtSizeGb: (bytes: number) => string;
}>();

const emit = defineEmits([
  'close',
  'update:managerSearch',
  'update:managerTab',
  'update:selectedModel',
  'deleteOllamaModel',
  'activateGguf',
  'moveGguf',
  'deleteGguf',
  'pullModel',
  'importGguf'
]);

const localManagerSearch = ref(props.managerSearch);
const localManagerTab = ref(props.managerTab);

const hfSearchQuery = ref('');
const hfSearchResults = ref<HuggingFaceModel[]>([]);
const hfSearching = ref(false);
const hfExpandedRepo = ref<string | null>(null);
const hfRepoFiles = ref<Record<string, HuggingFaceFile[]>>({});
const hfLoadingFiles = ref<Record<string, boolean>>({});

let searchTimeout: any = null;

async function doHfSearch(queryOverride?: string) {
  const q = queryOverride !== undefined ? queryOverride : (hfSearchQuery.value || localManagerSearch.value || '');
  hfSearching.value = true;
  try {
    hfSearchResults.value = await props.searchHuggingFace(q.trim());
  } catch (err) {
    console.error('HF search error:', err);
    hfSearchResults.value = [];
  } finally {
    hfSearching.value = false;
  }
}

async function toggleHfRepo(repoId: string) {
  if (hfExpandedRepo.value === repoId) {
    hfExpandedRepo.value = null;
    return;
  }
  hfExpandedRepo.value = repoId;
  if (!hfRepoFiles.value[repoId]) {
    hfLoadingFiles.value[repoId] = true;
    try {
      hfRepoFiles.value[repoId] = await props.listHuggingFaceFiles(repoId);
    } catch (err) {
      console.error('HF files error:', err);
      hfRepoFiles.value[repoId] = [];
    } finally {
      hfLoadingFiles.value[repoId] = false;
    }
  }
}

onMounted(() => {
  doHfSearch('');
});

watch(localManagerSearch, (val) => {
  emit('update:managerSearch', val);
  if (localManagerTab.value === 'huggingface') {
    hfSearchQuery.value = val;
    clearTimeout(searchTimeout);
    searchTimeout = setTimeout(() => {
      doHfSearch(val);
    }, 350);
  }
});
watch(() => props.managerSearch, (val) => { localManagerSearch.value = val; });

watch(localManagerTab, (val) => {
  emit('update:managerTab', val);
  if (val === 'huggingface' && hfSearchResults.value.length === 0) {
    doHfSearch(localManagerSearch.value);
  }
});
function fmtRate(bps?: number): string {
  if (!bps) return '';
  if (bps >= 1024 * 1024 * 1024) return `${(bps / (1024 * 1024 * 1024)).toFixed(1)} GB/s`;
  if (bps >= 1024 * 1024) return `${(bps / (1024 * 1024)).toFixed(1)} MB/s`;
  if (bps >= 1024) return `${(bps / 1024).toFixed(0)} KB/s`;
  return `${bps} B/s`;
}

function fmtEta(secs?: number): string {
  if (!secs || secs <= 0) return '';
  if (secs < 60) return `${secs}s`;
  const m = Math.floor(secs / 60);
  const s = secs % 60;
  return `${m}m ${s}s`;
}

</script>

<style scoped>
.active-download-banner {
  margin-top: 14px;
  padding: 12px 14px;
  border-radius: 10px;
  background: linear-gradient(135deg, rgba(220, 38, 38, 0.22), rgba(124, 58, 237, 0.22));
  border: 1px solid rgba(248, 113, 113, 0.35);
  box-shadow: 0 4px 18px rgba(0, 0, 0, 0.35);
  display: flex;
  flex-direction: column;
  gap: 8px;
}

.banner-top {
  display: flex;
  justify-content: space-between;
  align-items: center;
  gap: 12px;
}

.banner-title-group {
  display: flex;
  align-items: center;
  gap: 8px;
  min-width: 0;
}

.banner-pulse {
  width: 8px;
  height: 8px;
  border-radius: 50%;
  background: #f87171;
  box-shadow: 0 0 8px #f87171;
  animation: pulse-glow 1.5s infinite ease-in-out;
}

@keyframes pulse-glow {
  0%, 100% { transform: scale(1); opacity: 1; }
  50% { transform: scale(1.4); opacity: 0.6; }
}

.banner-label {
  font-size: 0.72rem;
  font-weight: 800;
  color: #fca5a5;
  letter-spacing: 0.05em;
}

.banner-model-name {
  font-size: 0.8rem;
  color: #fff;
  font-family: var(--font-mono, monospace);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.banner-stats {
  display: flex;
  align-items: center;
  gap: 10px;
  font-size: 0.76rem;
  color: rgba(255, 255, 255, 0.85);
  font-weight: 600;
  flex-shrink: 0;
}

.banner-pct {
  color: #c084fc;
  font-weight: 800;
  font-size: 0.86rem;
}

.banner-progress-track {
  width: 100%;
  height: 6px;
  background: rgba(0, 0, 0, 0.4);
  border-radius: 3px;
  overflow: hidden;
  position: relative;
}

.banner-progress-fill {
  height: 100%;
  background: linear-gradient(90deg, #dc2626, #a855f7);
  border-radius: 3px;
  transition: width 200ms ease;
}

.banner-progress-fill.indeterminate {
  width: 30% !important;
  animation: indeterminate-slide 1.5s infinite linear;
}

@keyframes indeterminate-slide {
  0% { transform: translateX(-100%); }
  100% { transform: translateX(400%); }
}

.banner-subtext {
  display: flex;
  justify-content: space-between;
  align-items: center;
  font-size: 0.72rem;
  color: rgba(255, 255, 255, 0.6);
}

.model-manager-overlay {
  padding: 24px;
  background: rgba(0, 0, 0, 0.8);
  backdrop-filter: blur(24px) saturate(1.4);
}

.manager-panel {
  width: min(1120px, 94vw);
  height: min(820px, 86vh);
  display: flex;
  flex-direction: column;
  overflow: hidden;
  border: 1px solid rgba(168, 85, 247, 0.3);
  border-radius: 20px;
  background: #0d0818;
  box-shadow: 0 24px 70px rgba(0, 0, 0, 0.95);
}

.manager-header {
  flex-shrink: 0;
  padding: 18px 22px 14px;
  border-bottom: 1px solid rgba(168, 85, 247, 0.25);
  background: rgba(16, 9, 28, 0.98);
  z-index: 10;
}

.manager-title-row {
  display: flex;
  align-items: center;
  gap: 12px;
}

.manager-glyph {
  width: 36px;
  height: 36px;
  display: grid;
  place-items: center;
  border-radius: 8px;
  background: linear-gradient(135deg, rgba(168, 85, 247, 0.8), rgba(99, 102, 241, 0.8));
  box-shadow: 0 4px 14px rgba(168, 85, 247, 0.25);
}

.manager-title {
  margin: 0;
  letter-spacing: 0.04em;
  font-size: 1rem;
  font-weight: 700;
}

.manager-subtitle {
  margin: 2px 0 0;
  color: rgba(255, 255, 255, 0.5);
  font-size: 0.8rem;
}

.manager-close {
  margin-left: auto;
  width: 32px;
  height: 32px;
  border-radius: 8px;
  border: 1px solid rgba(255, 255, 255, 0.1);
  color: rgba(255, 255, 255, 0.7);
  background: rgba(255, 255, 255, 0.06);
  cursor: pointer;
  display: flex;
  align-items: center;
  justify-content: center;
  transition: all 0.2s ease;
}

.manager-close:hover {
  background: rgba(239, 68, 68, 0.2);
  border-color: rgba(239, 68, 68, 0.4);
  color: white;
}

.manager-search-row {
  margin-top: 14px;
}

.manager-search {
  width: 100%;
  height: 40px;
  padding: 0 14px;
  border: 1px solid rgba(255, 255, 255, 0.08);
  border-radius: 8px;
  color: white;
  background: rgba(0, 0, 0, 0.3);
  font-size: 0.88rem;
  transition: border-color var(--duration-fast) ease, background var(--duration-fast) ease;
}

.manager-search:focus {
  outline: none;
  border-color: rgba(168, 85, 247, 0.5);
  background: rgba(0, 0, 0, 0.45);
}

.manager-tabs {
  display: grid;
  grid-template-columns: repeat(3, minmax(0, 1fr));
  gap: 8px;
  margin-top: 16px;
  padding: 6px;
  border-radius: 14px;
  border: 1px solid rgba(255, 255, 255, 0.08);
  background: rgba(0, 0, 0, 0.24);
}

.manager-tab {
  min-height: 42px;
  border: 1px solid transparent;
  border-radius: 11px;
  color: rgba(255, 255, 255, 0.7);
  cursor: pointer;
  background: transparent;
  font-weight: 800;
  letter-spacing: 0;
  transition: transform 0.18s ease, color 0.18s ease, border-color 0.18s ease, background 0.18s ease;
}

.manager-tab:hover {
  color: white;
  transform: translateY(-1px);
  border-color: rgba(255, 255, 255, 0.12);
  background: rgba(255, 255, 255, 0.05);
}

.manager-tab.active {
  color: white;
  border-color: rgba(248, 113, 113, 0.44);
  background: linear-gradient(135deg, rgba(220, 38, 38, 0.62), rgba(124, 58, 237, 0.5));
  box-shadow: 0 14px 32px rgba(124, 58, 237, 0.18);
}

.manager-tab-count {
  display: inline-grid;
  min-width: 24px;
  height: 22px;
  place-items: center;
  margin-left: 8px;
  padding: 0 7px;
  border-radius: 999px;
  background: rgba(0, 0, 0, 0.25);
}

.manager-body {
  flex: 1 1 0;
  min-height: 0;
  overflow-y: auto;
  overflow-x: auto;
  padding: 14px 22px;
}

.manager-disk-bar,
.manager-hint,
.manager-empty,
.model-card {
  border: 1px solid rgba(255, 255, 255, 0.1);
  background: linear-gradient(180deg, rgba(255, 255, 255, 0.07), rgba(255, 255, 255, 0.025));
  box-shadow: 0 18px 48px rgba(0, 0, 0, 0.28), inset 0 1px 0 rgba(255, 255, 255, 0.06);
  backdrop-filter: blur(16px);
}

.manager-disk-bar {
  display: flex;
  justify-content: space-between;
  gap: 12px;
  align-items: center;
  padding: 13px 15px;
  border-radius: 14px;
}

.manager-disk-label {
  color: rgba(255, 255, 255, 0.58);
  font-size: 0.74rem;
  font-weight: 900;
  letter-spacing: 0.12em;
}

.manager-disk-value {
  color: #fda4af;
  font-weight: 900;
}

.manager-hint {
  margin: 14px 0;
  padding: 13px 15px;
  border-radius: 14px;
  color: rgba(255, 255, 255, 0.68);
}

.manager-empty {
  margin-top: 16px;
  padding: 34px 18px;
  border-radius: 16px;
  color: rgba(255, 255, 255, 0.62);
  text-align: center;
}

.manager-list {
  display: grid;
  gap: 10px;
  margin-top: 12px;
}

.model-card {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
  padding: 12px 14px;
  border-radius: 12px;
  transition: transform 0.18s ease, border-color 0.18s ease, box-shadow 0.18s ease;
}

.model-card:hover {
  transform: translateY(-1px);
  border-color: rgba(167, 139, 250, 0.4);
  box-shadow: 0 12px 32px rgba(0, 0, 0, 0.35);
}

/* HuggingFace cards stack vertically */
.hf-model-card {
  flex-direction: column;
  align-items: stretch;
  gap: 0;
}

.model-card-main {
  min-width: 0;
  display: flex;
  align-items: center;
  gap: 12px;
}

.hf-card-main {
  cursor: pointer;
  width: 100%;
}

.model-card-icon {
  width: 34px;
  height: 34px;
  display: grid;
  place-items: center;
  flex: 0 0 auto;
  border-radius: 8px;
  color: white;
  background: linear-gradient(135deg, rgba(220, 38, 38, 0.86), rgba(124, 58, 237, 0.74));
  box-shadow: inset 0 1px 0 rgba(255, 255, 255, 0.16);
  font-weight: 900;
  font-size: 0.72rem;
}

.file-icon {
  background: linear-gradient(135deg, rgba(20, 184, 166, 0.72), rgba(124, 58, 237, 0.64));
}

.hf-icon {
  background: linear-gradient(135deg, #f59e0b, #d97706);
}

.model-card-info {
  min-width: 0;
  flex: 1 1 0;
}

.model-card-name {
  color: white;
  font-weight: 800;
  font-size: 0.88rem;
  overflow-wrap: anywhere;
  word-break: break-word;
  line-height: 1.25;
}

.model-card-meta {
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
  margin-top: 5px;
}

.model-tag {
  max-width: min(58vw, 520px);
  overflow: hidden;
  padding: 3px 8px;
  border: 1px solid rgba(255, 255, 255, 0.08);
  border-radius: 6px;
  color: rgba(255, 255, 255, 0.7);
  background: rgba(255, 255, 255, 0.05);
  font-size: 0.74rem;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.size-tag {
  background: rgba(168, 85, 247, 0.15);
  color: #c084fc;
  border-color: rgba(168, 85, 247, 0.3);
  font-weight: 700;
}

.download-tag {
  color: #fbbf24;
  background: rgba(251, 191, 36, 0.1);
  border-color: rgba(251, 191, 36, 0.2);
}

.likes-tag {
  color: #f43f5e;
  background: rgba(244, 63, 94, 0.1);
  border-color: rgba(244, 63, 94, 0.2);
}

.files-tag {
  color: #38bdf8;
  background: rgba(56, 189, 248, 0.12);
  border-color: rgba(56, 189, 248, 0.2);
  font-weight: 700;
}

.hf-toggle-btn {
  margin-left: 12px;
  flex-shrink: 0;
  font-size: 0.78rem;
  white-space: nowrap;
}

.hf-search-bar {
  margin-bottom: 14px;
  display: flex;
  gap: 10px;
}

.hf-search-btn {
  white-space: nowrap;
  padding: 0 20px;
}

.hf-files-container {
  border-top: 1px solid rgba(255, 255, 255, 0.08);
  padding-top: 12px;
  margin-top: 10px;
  width: 100%;
  box-sizing: border-box;
}

.quant-grid {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(185px, 1fr));
  gap: 8px;
  width: 100%;
  box-sizing: border-box;
}

.quant-card {
  background: rgba(255, 255, 255, 0.03);
  border: 1px solid rgba(255, 255, 255, 0.08);
  border-radius: 10px;
  padding: 10px;
  display: flex;
  flex-direction: column;
  gap: 8px;
  min-width: 0;
  box-sizing: border-box;
}

.quant-filename {
  font-weight: 700;
  font-size: 0.82rem;
  color: #e2e8f0;
  overflow-wrap: anywhere;
  word-break: break-word;
  line-height: 1.25;
}

.quant-details {
  display: flex;
  justify-content: space-between;
  align-items: center;
  gap: 4px;
  font-size: 0.76rem;
  color: rgba(255, 255, 255, 0.5);
  flex-wrap: wrap;
}

.quant-label-hl {
  color: #38bdf8;
}

.quant-size-pill {
  background: rgba(56, 189, 248, 0.12);
  color: #38bdf8;
  border: 1px solid rgba(56, 189, 248, 0.25);
  font-weight: 800;
  padding: 2px 7px;
  border-radius: 6px;
  font-size: 0.74rem;
}

.model-card-actions {
  display: flex;
  align-items: center;
  justify-content: flex-end;
  gap: 8px;
  flex-shrink: 0;
}

.model-action-btn,
.import-btn,
.close-modal-btn,
.model-active-badge {
  min-height: 32px;
  padding: 6px 10px;
  border: 1px solid rgba(255, 255, 255, 0.11);
  border-radius: 9px;
  color: white;
  background: rgba(255, 255, 255, 0.05);
  font-weight: 900;
  font-size: 0.78rem;
  letter-spacing: 0;
  cursor: pointer;
  transition: transform 0.18s ease, border-color 0.18s ease, box-shadow 0.18s ease, background 0.18s ease;
}

.model-action-btn:hover:not(:disabled),
.import-btn:hover:not(:disabled),
.close-modal-btn:hover {
  transform: translateY(-1px);
  border-color: rgba(248, 113, 113, 0.42);
  box-shadow: 0 12px 30px rgba(0, 0, 0, 0.32);
}

.model-action-btn:disabled,
.import-btn:disabled {
  cursor: not-allowed;
  opacity: 0.52;
}

.model-action-btn.use,
.import-btn {
  border-color: rgba(248, 113, 113, 0.42);
  background: linear-gradient(135deg, rgba(220, 38, 38, 0.82), rgba(124, 58, 237, 0.72));
}

.model-action-btn.success,
.model-active-badge {
  color: #bbf7d0;
  border-color: rgba(34, 197, 94, 0.38);
  background: rgba(34, 197, 94, 0.14);
}

.model-action-btn.danger {
  color: #fecaca;
  border-color: rgba(248, 113, 113, 0.24);
}

.model-action-btn.danger:hover:not(:disabled) {
  background: rgba(220, 38, 38, 0.18);
}

.quant-buttons {
  display: flex;
  justify-content: flex-end;
  gap: 8px;
  flex-wrap: wrap;
}

.quant-size {
  display: block;
  margin-top: 3px;
  font-size: 0.68rem;
  color: rgba(255, 255, 255, 0.72);
}

.model-action-btn.fit-tight {
  border-color: rgba(245, 158, 11, 0.54);
  background: linear-gradient(135deg, rgba(245, 158, 11, 0.78), rgba(124, 58, 237, 0.52));
}

.model-action-btn.fit-too-big {
  border-color: rgba(248, 113, 113, 0.62);
  background: linear-gradient(135deg, rgba(220, 38, 38, 0.76), rgba(76, 29, 149, 0.62));
}

.model-action-btn.use:not(.fit-tight):not(.fit-too-big) {
  border-color: rgba(34, 197, 94, 0.4);
}

.manager-footer {
  flex-shrink: 0;
  display: flex;
  justify-content: space-between;
  gap: 12px;
  padding: 14px 22px 16px;
  border-top: 1px solid rgba(255, 255, 255, 0.08);
  background: rgba(0, 0, 0, 0.18);
}

@media (max-width: 760px) {
  .model-manager-overlay {
    padding: 12px;
  }

  .manager-panel {
    width: 100%;
    height: 92vh;
    border-radius: 18px;
  }

  .manager-tabs {
    grid-template-columns: 1fr;
  }

  .manager-body {
    height: calc(100% - 280px);
    padding: 14px;
  }

  .model-card {
    align-items: stretch;
    flex-direction: column;
  }

  .model-card-actions,
  .quant-buttons {
    justify-content: stretch;
  }

  .model-action-btn {
    flex: 1;
  }
}
</style>
