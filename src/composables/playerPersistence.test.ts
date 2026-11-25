import { createPinia, setActivePinia } from 'pinia';
import { afterEach, expect, it, vi } from 'vitest';
import { useCollectionsStore } from '../features/collections/store';
import { playerStorage } from '../services/storage/playerStorage';
import { createPlayerPersistence } from './playerPersistence';

afterEach(() => {
  vi.restoreAllMocks();
  vi.useRealTimers();
});

it('protects playlists during async startup and persists intentional deletions afterwards', () => {
  setActivePinia(createPinia());
  vi.useFakeTimers();
  const write = vi.spyOn(playerStorage, 'writePlayerState').mockImplementation(() => {});
  const persistence = createPlayerPersistence({ keys: {
    playerPlaylistPaths: 'paths', playerQueuePaths: 'queue',
    legacyPlayerPlaylist: 'old-playlist', legacyPlayerQueue: 'old-queue',
  } });
  persistence.schedulePersistedState();
  vi.advanceTimersByTime(250);
  expect(write).not.toHaveBeenCalled();

  const collections = useCollectionsStore();
  collections.setPlaylists([{ id: 'old', name: 'Recovered', createdAt: '', songPaths: ['song.flac'] }]);
  persistence.enablePersistence();
  persistence.flushPersistedState();
  expect(write.mock.lastCall?.[0].playlists[0]?.songPaths).toEqual(['song.flac']);
  collections.deletePlaylist('old');
  persistence.flushPersistedState();
  expect(write.mock.lastCall?.[0].playlists).toEqual([]);
});
