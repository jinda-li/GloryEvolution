import type { DayStats } from "../types";

export const HEATMAP_WEEKS = 20;

export type HeatmapCell = {
  key: string;
  words: number;
  /** 0 = no dictation, 1–4 = relative activity. */
  level: number;
  future: boolean;
};

export type Insights = {
  activeDays: number;
  currentStreak: number;
  longestStreak: number;
  /** Columns are weeks (oldest first), rows run Monday to Sunday. */
  weeks: HeatmapCell[][];
};

export function dayKey(date: Date) {
  const month = String(date.getMonth() + 1).padStart(2, "0");
  const day = String(date.getDate()).padStart(2, "0");
  return `${date.getFullYear()}-${month}-${day}`;
}

function addDays(date: Date, days: number) {
  const next = new Date(date);
  next.setDate(next.getDate() + days);
  return next;
}

export function computeInsights(daily: Record<string, DayStats>, today = new Date()): Insights {
  const active = (date: Date) => (daily[dayKey(date)]?.sessions ?? 0) > 0;
  const keys = Object.keys(daily).filter((key) => daily[key].sessions > 0).sort();

  // A streak still counts if today has no dictation yet but yesterday did.
  let currentStreak = 0;
  let cursor = active(today) ? today : addDays(today, -1);
  while (active(cursor)) {
    currentStreak += 1;
    cursor = addDays(cursor, -1);
  }

  let longestStreak = 0;
  let run = 0;
  let previous: Date | null = null;
  for (const key of keys) {
    const [year, month, day] = key.split("-").map(Number);
    const date = new Date(year, month - 1, day);
    run = previous && dayKey(addDays(previous, 1)) === key ? run + 1 : 1;
    longestStreak = Math.max(longestStreak, run);
    previous = date;
  }

  const mondayOffset = (today.getDay() + 6) % 7;
  const start = addDays(today, -mondayOffset - (HEATMAP_WEEKS - 1) * 7);
  const max = Math.max(1, ...keys.map((key) => daily[key].words));
  const weeks: HeatmapCell[][] = [];
  for (let week = 0; week < HEATMAP_WEEKS; week += 1) {
    const column: HeatmapCell[] = [];
    for (let weekday = 0; weekday < 7; weekday += 1) {
      const date = addDays(start, week * 7 + weekday);
      const key = dayKey(date);
      const stats = daily[key];
      const words = stats?.words ?? 0;
      const level = stats && stats.sessions > 0 ? Math.min(4, 1 + Math.floor((words / max) * 4)) : 0;
      column.push({ key, words, level, future: date > today });
    }
    weeks.push(column);
  }

  return { activeDays: keys.length, currentStreak, longestStreak, weeks };
}
