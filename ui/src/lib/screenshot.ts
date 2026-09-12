import { invoke } from '@tauri-apps/api/core';

export async function copyAppScreenshot() {
  await document.fonts.ready;
  await invoke('copy_screenshot');
}
