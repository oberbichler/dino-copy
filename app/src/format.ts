// Human-readable numbers for the window, in the user's language.

import { language, t } from "./i18n";

/** 1234567 → "1.2 MB" (decimal units, like Finder; "1,2 Mo" in French). */
export function bytes(n: number): string {
  if (n < 1000) return t("bytes", { n });
  const units = t("units").split(" ");
  let u = -1;
  let v = n;
  while (v >= 1000 && u < units.length - 1) {
    v /= 1000;
    u++;
  }
  const digits = v >= 100 ? 0 : 1;
  const num = v.toLocaleString(language(), { minimumFractionDigits: digits, maximumFractionDigits: digits });
  return `${num} ${units[u]}`;
}

/** 1234567 → "1,234,567" ("1.234.567" in German). */
export function count(n: number): string {
  return n.toLocaleString(language());
}

/** Remaining time, rounded the way people say it. */
export function timeLeft(secs: number): string {
  if (secs < 45) return t("etaUnderMinute");
  const mins = Math.round(secs / 60);
  if (mins < 60) return t("etaMinutes", { n: mins });
  const h = Math.floor(mins / 60);
  const m = mins % 60;
  return m === 0 ? t("etaHours", { h }) : t("etaHoursMinutes", { h, m });
}

/** Elapsed time: "42 s", "3 min 5 s", "1 h 12 min". */
export function duration(secs: number): string {
  const s = Math.round(secs);
  if (s < 60) return `${s} s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m} min ${s % 60} s`;
  return `${Math.floor(m / 60)} h ${m % 60} min`;
}
