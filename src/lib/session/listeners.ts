/** Keyed listener sets: the subscribe half of every per-session store. */

export function addListener<F>(map: Map<string, Set<F>>, key: string, cb: F): () => void {
  let set = map.get(key)
  if (!set) {
    set = new Set()
    map.set(key, set)
  }
  set.add(cb)
  return () => {
    set.delete(cb)
    if (set.size === 0) map.delete(key)
  }
}

export function notifyListeners(map: Map<string, Set<() => void>>, key: string): void {
  const set = map.get(key)
  if (set) for (const cb of set) cb()
}
