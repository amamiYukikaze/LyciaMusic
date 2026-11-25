<script setup lang="ts">
import { onScopeDispose, ref, watch } from 'vue';
import { ChevronLeft, ListMusic, Music2, Play } from 'lucide-vue-next';
import { remoteLibraryApi } from '../../services/tauri/remoteLibraryApi';
import { useLibrarySongResolver } from '../../composables/useLibrarySongResolver';
import { usePlaybackController } from '../../features/playback/usePlaybackController';
import { useToast } from '../../composables/toast';
import type { RemoteFileEntry } from '../../types';

const props = defineProps<{ sourceId: string }>();
const list = ref<RemoteFileEntry | null>(null);
const entries = ref<RemoteFileEntry[]>([]);
const loading = ref(false);
const error = ref('');
const { loadSong } = useLibrarySongResolver();
const { clearQueue, addSongPathsToQueue, playSong } = usePlaybackController();
const { showToast } = useToast();
let generation = 0;
const browse = async (selected: RemoteFileEntry | null) => {
  const request = ++generation;
  list.value = selected;
  entries.value = [];
  error.value = '';
  loading.value = true;
  try {
    const rows = await remoteLibraryApi.listRemoteDirectory(props.sourceId, selected?.remotePath ?? '/');
    if (request === generation) entries.value = rows;
  } catch (cause) {
    if (request === generation) error.value = String(cause);
  } finally {
    if (request === generation) loading.value = false;
  }
};
watch(() => props.sourceId, () => void browse(null), { immediate: true });
onScopeDispose(() => { ++generation; });
const play = async (index: number) => {
  const request = generation;
  const paths = entries.value.slice(index).map(row => `remote://${props.sourceId}${row.remotePath}`);
  if (!paths.length) return;
  try {
    const song = await loadSong(paths[0]);
    if (request !== generation) return;
    if (!song) { showToast('请先同步这座远程音乐库', 'info'); return; }
    await clearQueue();
    if (request !== generation) return;
    addSongPathsToQueue(paths);
    await playSong(song, { preserveQueue: true });
  } catch { showToast('远程歌单播放失败', 'error'); }
};
</script>

<template>
  <section class="server-playlists custom-scrollbar" aria-label="服务器歌单">
    <header v-if="list">
      <button type="button" aria-label="返回服务器歌单" @click="browse(null)"><ChevronLeft :size="18" />全部歌单</button>
      <strong>{{ list.name }}</strong>
      <button v-if="entries.length" type="button" @click="play(0)"><Play :size="16" />播放全部</button>
    </header>
    <p v-if="loading" role="status">正在读取歌单…</p>
    <p v-else-if="error" role="alert">{{ error }} <button type="button" @click="browse(list)">重试</button></p>
    <p v-else-if="!entries.length">{{ list ? '这张歌单为空' : '服务器上还没有歌单' }}</p>
    <button v-for="(entry, index) in entries" :key="`${index}:${entry.remotePath}`" type="button" class="entry" @click="entry.isDir ? browse(entry) : play(index)">
      <component :is="entry.isDir ? ListMusic : Music2" :size="17" /><span>{{ entry.name }}</span>
    </button>
  </section>
</template>

<style scoped>
.server-playlists { flex: 1; min-height: 0; overflow: auto; padding: 0 0 20px; }
header, button { display: flex; align-items: center; gap: 8px; }
header { flex-wrap: wrap; padding: 10px 0; gap: 16px; }
button { cursor: pointer; }
.entry { width: 100%; padding: 10px; text-align: left; border-bottom: 1px solid var(--line); }
.entry:hover, button:focus-visible { background: var(--hover); }
</style>
