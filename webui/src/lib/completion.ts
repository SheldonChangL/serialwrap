/**
 * Tab completion for the write bar, fed by what the device itself has
 * printed.
 *
 * The insight this is built on: the paths an operator wants to type are
 * almost always paths the console has already shown — `ls` listings, log
 * lines naming `/mnt/mmc/DCIM/A/…`, error messages. So instead of asking
 * the device anything (which would mean extra writes, gate/audit noise,
 * and a character-mode terminal — see the write bar's doc comment for why
 * it is deliberately line-buffered), the GUI harvests absolute paths from
 * the RX stream as it renders, and Tab completes against that index.
 *
 * Completion is segment-wise, like a shell: `/mn` → `/mnt/`, another Tab
 * → `/mnt/mmc/`, so one long harvested path offers every directory along
 * the way. A word with no leading `/` is matched against individual path
 * segments (`DCI` → `DCIM`), which is what makes `ls DCIM`-style relative
 * commands completable without indiscriminately indexing every word the
 * device ever printed (debug-log prose would drown the useful candidates).
 *
 * The index is per device id and bounded; it lives at module scope so it
 * survives the `{#key}` remounts `App.svelte` does when switching devices.
 */

/** Insertion-ordered per device; oldest evicted past the cap. */
const pathsByDevice = new Map<string, Set<string>>();

const MAX_PATHS_PER_DEVICE = 500;
/** Longer than any path a human is completing toward; keeps a binary blob
 * that happened to contain slashes from bloating the index. */
const MAX_PATH_LENGTH = 200;

/** Absolute POSIX paths as firmware consoles actually print them. `+`,
 * `:` and `,` are deliberately excluded so `path:line` diagnostics and
 * comma-joined lists split cleanly. */
const PATH_RE = /\/(?:[\w.-]+\/)+[\w.-]*|\/[\w.-]+/g;

/** Record every absolute path in `text` (one already-ANSI-stripped line of
 * device output) into `deviceId`'s index. */
export function harvest(deviceId: string, text: string): void {
  if (!text.includes("/")) return;
  let set = pathsByDevice.get(deviceId);
  if (!set) {
    set = new Set();
    pathsByDevice.set(deviceId, set);
  }
  for (const m of text.matchAll(PATH_RE)) {
    const p = m[0];
    if (p.length < 2 || p.length > MAX_PATH_LENGTH) continue;
    // Re-inserting moves it to the back of the eviction order, so paths
    // the device keeps mentioning stay indexed.
    set.delete(p);
    set.add(p);
  }
  while (set.size > MAX_PATHS_PER_DEVICE) {
    const oldest = set.values().next().value;
    if (oldest === undefined) break;
    set.delete(oldest);
  }
}

/** Candidates for the word under the cursor, shell-style.
 *
 * - `word` starting with `/`: expand to the next segment boundary among
 *   indexed paths (`/mn` → `/mnt/`), or the whole path once no further
 *   `/` remains.
 * - bare `word`: complete against individual segments of indexed paths
 *   (`DCI` → `DCIM`).
 *
 * Sorted shortest-first so cycling starts at the least-committal option.
 */
export function completeWord(deviceId: string, word: string): string[] {
  if (word === "") return [];
  const paths = pathsByDevice.get(deviceId);
  if (!paths) return [];
  const out = new Set<string>();
  if (word.startsWith("/")) {
    for (const p of paths) {
      if (!p.startsWith(word) || p === word) continue;
      const rest = p.slice(word.length);
      const slash = rest.indexOf("/");
      out.add(slash >= 0 ? word + rest.slice(0, slash + 1) : p);
    }
  } else {
    for (const p of paths) {
      for (const seg of p.split("/")) {
        if (seg.startsWith(word) && seg !== word) out.add(seg);
      }
    }
  }
  return [...out].sort((a, b) => a.length - b.length || a.localeCompare(b));
}

/** Longest common prefix of `candidates` — what a first Tab press extends
 * the word to when several completions remain. */
export function longestCommonPrefix(candidates: string[]): string {
  if (candidates.length === 0) return "";
  let prefix = candidates[0];
  for (const c of candidates.slice(1)) {
    let i = 0;
    while (i < prefix.length && i < c.length && prefix[i] === c[i]) i++;
    prefix = prefix.slice(0, i);
    if (prefix === "") break;
  }
  return prefix;
}
