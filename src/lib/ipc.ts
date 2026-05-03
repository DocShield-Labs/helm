/**
 * Typed Tauri command wrappers, re-exported from the specta-generated bindings.
 * Always import from here, never from `@tauri-apps/api/core` directly, so we
 * keep one chokepoint between the frontend and the Rust side.
 */

export { commands } from '@bindings'
export type { PingResponse } from '@bindings'
