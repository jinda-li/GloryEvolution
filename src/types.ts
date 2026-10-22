export type RecordMode = "toggle" | "hold";

export type AppSettings = {
  apiKey: string;
  baseUrl: string;
  sttModel: string;
  llmModel: string;
  polishEnabled: boolean;
  language: string;
  shortcut: string;
  translateShortcut: string;
  translateLanguage: string;
  recordMode: RecordMode;
  autoPaste: boolean;
  inputDevice: string;
  dictionary: string;
  systemPrompt: string;
  soundEnabled: boolean;
  historyEnabled: boolean;
  launchAtLogin: boolean;
  onboarded: boolean;
};

export type RecordingStatus = "idle" | "recording" | "processing" | "done" | "error" | "failed" | "preview";

export type RecordingEvent = {
  status: RecordingStatus;
  message: string;
};

export type RecorderHintEvent = {
  message: string;
};

export type AudioLevelEvent = {
  level: number;
};

export type ProcessingProgressEvent = {
  phase: string;
  message: string;
  progress: number;
};

export type LatencyMetricsEvent = {
  totalMs: number;
  sttMs: number;
  optimizeMs: number;
  pasteMs: number;
};

export type TextPreviewEvent = {
  text: string;
};

export type ProcessResult = {
  rawText: string;
  optimizedText: string;
  durationSeconds: number;
  delivery: "inserted" | "needsCopy";
  warning: string | null;
};

export type HistoryEntry = {
  id: string;
  createdAt: number;
  rawText: string;
  text: string;
  durationSeconds: number;
  polished: boolean;
  translatedTo?: string;
};

export type UsageStats = {
  totalSeconds: number;
  totalWords: number;
  totalSessions: number;
  daily: Record<string, DayStats>;
};

export type DayStats = {
  words: number;
  seconds: number;
  sessions: number;
};

export type HistoryStore = {
  stats: UsageStats;
  entries: HistoryEntry[];
};

export type KeyStatus = {
  label: string;
  usage: number;
  limit: number | null;
  limitRemaining: number | null;
  isFreeTier: boolean;
  creditsRemaining: number | null;
};

export type AppInfo = {
  version: string;
  defaultSystemPrompt: string;
};
