// Browser-only preview (npm run dev without Tauri): fakes the backend so pages render.
import { mockIPC, mockWindows } from "@tauri-apps/api/mocks";

import { DEFAULT_SETTINGS } from "./constants";

function mockDaily(now: number) {
  const daily: Record<string, { words: number; seconds: number; sessions: number }> = {};
  for (let i = 0; i < 90; i += 1) {
    if (i % 7 === 3 || i % 11 === 5) {
      continue;
    }
    const date = new Date(now - i * 86_400_000);
    const key = `${date.getFullYear()}-${String(date.getMonth() + 1).padStart(2, "0")}-${String(date.getDate()).padStart(2, "0")}`;
    const words = ((i * 37) % 400) + 20;
    daily[key] = { words, seconds: words / 3, sessions: 1 + (i % 5) };
  }
  return daily;
}

export function installDevMock() {
  mockWindows(window.location.pathname === "/recorder" ? "recorder" : "main");
  const now = Date.now();
  mockIPC((cmd) => {
    switch (cmd) {
      case "load_settings":
        return { ...DEFAULT_SETTINGS, apiKey: "sk-or-v1-demo", onboarded: !location.search.includes("onboarding") };
      case "app_info":
        return { version: "0.2.0", defaultSystemPrompt: "（默认润色指令）" };
      case "get_history":
        return {
          stats: { totalSeconds: 1834, totalWords: 5210, totalSessions: 142, daily: mockDaily(now) },
          entries: [
            { id: "0", createdAt: now - 30_000, rawText: "帮我跟他说明天的会改到四点", text: "Could we move tomorrow's meeting to 4 pm?", durationSeconds: 4.0, polished: true, translatedTo: "en" },
            { id: "1", createdAt: now - 60_000, rawText: "嗯那个我们明天下午三点开会呃不对是四点", text: "我们明天下午四点开会。", durationSeconds: 5.2, polished: true },
            { id: "2", createdAt: now - 86_400_000, rawText: "ship the tory app on friday", text: "Ship the Tauri app on Friday.", durationSeconds: 3.1, polished: true },
          ],
        };
      case "list_input_devices":
        return ["麦克风 (Realtek High Definition Audio)", "耳机 (AirPods Pro)"];
      case "test_connection":
        return { label: "demo", usage: 0.1, limit: null, limitRemaining: null, isFreeTier: false, creditsRemaining: 4.2 };
      default:
        return null;
    }
  }, { shouldMockEvents: true });
}
